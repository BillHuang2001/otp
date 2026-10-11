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

//! `ets:select_replace/2` and the **body runtime-failure** rule, every case
//! cross-checked against the system `erl` (OTP 29.1.1 / erts-17.1).
//!
//! # How the oracle renderings were obtained
//!
//! Each expectation below was produced by running the identical spec against the
//! system `erl` (`erl -noshell -eval 'probe:run()' -s init stop`) through a probe
//! module that uses the same canonical renderer as
//! `tests/differential/erl_program.rs`: small integers, atoms, tuples and proper
//! lists only. `srt/1` renders a term list **sorted** (used for `set`/`bag`/
//! `duplicate_bag`, whose traversal order is unspecified), `inorder/1` keeps it
//! (used for `ordered_set`, whose order *is* the contract), `cap/1` renders a
//! possibly-raising call as `badarg` or `ok:<rendered>`, and the battery composes
//! each case as `{<outcome>,<rendered table>}`. The oracle's line is quoted next to
//! each assertion.
//!
//! # The two rules under test
//!
//! 1. **`ets:select_replace/2`** replaces every matched object with its clause
//!    body's value and returns the number replaced. The replacement must be a tuple
//!    of arity `>= keypos` whose `keypos` element **equals** the matched object's
//!    key, otherwise the whole call is `badarg` and the table is left
//!    **unchanged** — so every replacement is validated before the first mutation.
//!    Supported on `set`, `ordered_set` and `duplicate_bag`; on `bag` the call is
//!    `badarg` up front, whatever the spec (`ets_select_replace_2` refuses `DB_BAG`
//!    before it looks at anything).
//! 2. **A body expression that fails at runtime** is replaced by the atom `EXIT`
//!    and evaluation continues, so the clause's value is its *last* expression's
//!    value. This is the C `FAIL_TERM = am_EXIT`, which the `matchCall*` operations
//!    substitute when `matchCatch` (which compiles every **body**) has set
//!    `do_catch` — `erts/emulator/beam/erl_db_util.c`. A **guard** failure still
//!    skips the clause (guards do *not* compile under `matchCatch`).
//!
//! # The oracle's *static* body check
//!
//! `erl` validates the body's **shape** statically (`db_match_keeps_key`,
//! `erts/emulator/beam/erl_db_util.c:1422`, installed as the match spec's
//! `on_match_validation`): the body must be a one-expression list whose single
//! expression is `'$_'`, the `{{ … }}` construction form, or `{const, {…}}`, and
//! the constructed key must be *syntactically* the head's key (or
//! `{element, KeyPos, '$_'}`). It therefore raises `badarg` **even when nothing
//! matches**, and `beam_ets` adopts exactly that rule (the crate's
//! `static_keeps_key`, mirroring `db_match_keeps_key`); the `static_check_*` tests
//! at the end of this file pin the rejected shapes and the unchanged table.
//!
//! This is deliberately one comprehensive suite: it is a long file (over the
//! workspace's ~1000-line guidance) because `select_replace/2`, the body
//! runtime-failure rule and the static-check rejections are covered as a single
//! battery against one shared oracle renderer; splitting it by rule would
//! duplicate that harness.

mod common;

use beam_common::{BeamError, Result};
use beam_ets::{compile, MatchSpec, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;
use common::*;

// ---------------------------------------------------------------------------
// Oracle renderer + spec builders (the same shape as the differential harness)
// ---------------------------------------------------------------------------

/// The atoms every spec below references. `EXIT` must be interned: a failed body
/// expression is replaced by that atom.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in [
        "_", "$_", "$$", "$1", "$2", "EXIT", "true", "false", "hd", "element", "const", ">", "==",
    ] {
        atoms.intern(name);
    }
    atoms
}

/// The atom term named `name` (interned by [`atoms`]).
fn at(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).expect("interned by `atoms`"))
}

/// `{Head, Guards, Body}` — one clause tuple.
fn clause(heap: &mut Heap, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
    let guard_list = proper_list(heap, guards);
    let body_list = proper_list(heap, body);
    tuple_of(heap, &[head, guard_list, body_list])
}

/// `[ {Head, Guards, Body} ]` — a one-clause spec.
fn spec1(heap: &mut Heap, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
    let clause = clause(heap, head, guards, body);
    proper_list(heap, &[clause])
}

/// A spec from already-built clause tuples.
fn spec_of(heap: &mut Heap, clauses: &[Eterm]) -> Eterm {
    proper_list(heap, clauses)
}

/// The `{{ … }}` body construction form: a one-element tuple wrapping a tuple.
fn construct(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    let inner = tuple_of(heap, elements);
    tuple_of(heap, &[inner])
}

/// The oracle's `r/1`: small integers, atoms, tuples and proper lists only.
fn render(atoms: &AtomTable, term: Eterm) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if is_atom(term) {
        return atoms
            .name(atom_val(term))
            .expect("every rendered atom is interned in `atoms`")
            .to_string();
    }
    if is_nil(term) {
        return "[]".to_string();
    }
    if is_list(term) {
        let mut parts = Vec::new();
        let mut cursor = term;
        while is_list(cursor) {
            // SAFETY: `is_list(cursor)` established a live cons cell, and the term
            // is test- or table-owned, so it outlives the walk.
            let (head, tail) = unsafe { (car(cursor), cdr(cursor)) };
            parts.push(render(atoms, head));
            cursor = tail;
        }
        assert!(is_nil(cursor), "the renderer supports proper lists only");
        return format!("[{}]", parts.join(","));
    }
    // SAFETY: `is_tuple` inspects the header word of a possibly-boxed term and
    // checks the boxed/arityval shape first, so it is sound for any value word.
    if unsafe { is_tuple(term) } {
        // SAFETY: `is_tuple` established a live tuple with a valid arityval header.
        let arity = unsafe { tuple_size(term) };
        let parts: Vec<String> = (0..arity)
            // SAFETY: every `i` is `< arity == tuple_size(term)`, exactly
            // `tuple_element`'s precondition.
            .map(|i| render(atoms, unsafe { tuple_element(term, i) }))
            .collect();
        return format!("{{{}}}", parts.join(","));
    }
    panic!("the renderer supports small integers, atoms, tuples and proper lists only");
}

/// `srt/1` — an order-independent rendering of a term list.
fn sorted_terms(atoms: &AtomTable, terms: &[Eterm]) -> String {
    let mut parts: Vec<String> = terms.iter().map(|term| render(atoms, *term)).collect();
    parts.sort();
    format!("[{}]", parts.join(","))
}

/// `inorder/1` — an order-preserving rendering of a term list.
fn inorder_terms(atoms: &AtomTable, terms: &[Eterm]) -> String {
    let parts: Vec<String> = terms.iter().map(|term| render(atoms, *term)).collect();
    format!("[{}]", parts.join(","))
}

/// `srt(ets:tab2list(T))`.
fn sorted_table(atoms: &AtomTable, table: &Table) -> String {
    sorted_terms(atoms, &table.tab2list())
}

/// `inorder(ets:tab2list(T))` — the `ordered_set` rendering.
fn inorder_table(atoms: &AtomTable, table: &Table) -> String {
    inorder_terms(atoms, &table.tab2list())
}

/// The battery's `t([C, tab(T)])`: `{<outcome>,<rendered table>}`.
fn composed(outcome: &str, table: &str) -> String {
    format!("{{{outcome},{table}}}")
}

/// The oracle's `cap/1` around `select_replace`: `ok:<count>` or `badarg`.
fn cap_count(result: Result<usize>) -> String {
    match result {
        Ok(count) => format!("ok:{count}"),
        Err(BeamError::BadArg) => "badarg".to_string(),
        Err(other) => panic!("select_replace failed with a non-`badarg` error: {other:?}"),
    }
}

/// A `select_count`/`select_delete` outcome: the bare count (`r(C)`).
fn count_outcome(result: Result<usize>) -> String {
    match result {
        Ok(count) => count.to_string(),
        Err(other) => panic!("expected a count, got: {other:?}"),
    }
}

/// A `select` outcome: `srt(R)` or, for `ordered_set`, `inorder(R)`.
fn select_outcome(atoms: &AtomTable, result: Result<Vec<Eterm>>, ordered: bool) -> String {
    match result {
        Ok(terms) if ordered => inorder_terms(atoms, &terms),
        Ok(terms) => sorted_terms(atoms, &terms),
        Err(other) => panic!("expected a select result, got: {other:?}"),
    }
}

/// The `beam_alloc::Heap` the select family builds fresh terms on (distinct from
/// the `beam_term` test arena the *source* terms live in).
fn output_heap() -> beam_alloc::Heap {
    beam_alloc::Heap::new(4096).expect("a test heap is allocatable")
}

/// The oracle's `t([cap(select_replace), srt(tab2list)])` line.
fn replace_line(atoms: &AtomTable, table: &mut Table, spec: &MatchSpec) -> String {
    let outcome = cap_count(table.select_replace(spec, atoms, &mut output_heap()));
    composed(&outcome, &sorted_table(atoms, table))
}

/// The same, rendering the table in traversal order (`ordered_set`).
fn replace_line_ordered(atoms: &AtomTable, table: &mut Table, spec: &MatchSpec) -> String {
    let outcome = cap_count(table.select_replace(spec, atoms, &mut output_heap()));
    composed(&outcome, &inorder_table(atoms, table))
}

/// A table of `kind` with `keypos`.
fn new_table_keypos(kind: TableKind, keypos: usize) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    opts.set_keypos(keypos).expect("keypos is >= 1");
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

// ---------------------------------------------------------------------------
// `ets:select_replace/2` — the oracle battery
// ---------------------------------------------------------------------------

#[test]
fn replace_that_matches_nothing_is_ok_with_count_zero() {
    // erl: T = set, {1,10}; `ets:select_replace(T, [{{99,'_'},[],[{{99,1}}]}])`
    //      -> `{ok:0,[{1,10}]}` — a valid (key-keeping) body that matches nothing.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[make_small(99), at(&atoms, "_")]);
    let body = construct(&mut heap, &[make_small(99), make_small(1)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:0,[{1,10}]}"
    );
}

#[test]
fn replace_identity_on_a_set_is_counted() {
    // erl: `ets:select_replace(T, [{{'$1','$2'},[],[{{'$1','$2'}}]}])` on
    //      {1,10}/{2,20} -> `{ok:2,[{1,10},{2,20}]}` (the replacement equals the
    //      matched object, so the table is unchanged — but both objects count).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{1,10},{2,20}]}"
    );
}

#[test]
fn replace_identity_on_an_ordered_set_is_counted() {
    // erl: `ets:select_replace(T, [{{'$1','$2'},[],[{{'$1','$2'}}]}])` on the
    //      ordered_set {1,10}/{2,20} -> `{ok:2,[{1,10},{2,20}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::OrderedSet);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line_ordered(&atoms, &mut t, &spec),
        "{ok:2,[{1,10},{2,20}]}"
    );
}

#[test]
fn replace_identity_on_a_duplicate_bag_is_counted() {
    // erl: `ets:select_replace(T, [{{'$1','$2'},[],[{{'$1','$2'}}]}])` on the
    //      duplicate_bag {1,10}/{1,20} -> `{ok:2,[{1,10},{1,20}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::DuplicateBag);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{1,10},{1,20}]}"
    );
}

#[test]
fn replace_on_a_bag_is_badarg_whatever_the_spec() {
    // erl: a `bag` refuses the kind up front, so a *matching* and a
    //      *never-matching* spec are both `badarg`, with the table unchanged:
    //      `{badarg,badarg,[{1,10},{2,20}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Bag);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let matching = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    let never_head = tuple_of(&mut heap, &[make_small(99), at(&atoms, "_")]);
    let never_body = construct(&mut heap, &[make_small(99), make_small(1)]);
    let never = compile(spec1(&mut heap, never_head, &[], &[never_body]), &atoms).unwrap();

    let first = cap_count(t.select_replace(&matching, &atoms, &mut output_heap()));
    let second = cap_count(t.select_replace(&never, &atoms, &mut output_heap()));
    assert_eq!(
        format!("{{{first},{second},{}}}", sorted_table(&atoms, &t)),
        "{badarg,badarg,[{1,10},{2,20}]}"
    );
}

#[test]
fn replace_with_an_arity_one_construction_shrinks_the_tuple() {
    // erl: body `{{'$1'}}` on {1,10}/{2,20} -> `{ok:2,[{1},{2}]}` (the key is
    //      still position 1 and still equal, so the shorter tuple is accepted).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[at(&atoms, "$1")]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{1},{2}]}"
    );
}

#[test]
fn replace_with_a_whole_object_body_is_the_identity_but_still_counts() {
    // erl: body `'$_'` on {1,10}/{2,20} -> `{ok:2,[{1,10},{2,20}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{1,10},{2,20}]}"
    );
}

#[test]
fn replace_updates_every_matched_object() {
    // erl: body `{{'$1',99}}` on {1,10}/{2,20} -> `{ok:2,[{1,99},{2,99}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), make_small(99)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{1,99},{2,99}]}"
    );
}

#[test]
fn replace_with_a_changed_key_is_badarg_and_changes_nothing() {
    // erl: body `{{7,'$2'}}` on {1,10} -> `{badarg,[{1,10}]}` (the replacement's
    //      key 7 differs from the matched key 1).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[make_small(7), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{badarg,[{1,10}]}"
    );
}

#[test]
fn replace_with_a_const_form_that_keeps_the_key_is_accepted() {
    // erl: `[{const, {1, 99}}]` with head `{1,'_'}` on {1,10}/{2,20} ->
    //      `{ok:1,[{1,99},{2,20}]}` (only the key-1 object matches).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[make_small(1), at(&atoms, "_")]);
    let constant = int_tuple(&mut heap, &[1, 99]);
    let body = tuple_of(&mut heap, &[at(&atoms, "const"), constant]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:1,[{1,99},{2,20}]}"
    );
}

#[test]
fn replace_with_a_const_form_that_changes_the_key_is_badarg() {
    // erl: `[{const, {1, 99}}]` with head `{'$1','_'}` on {1,10}/{2,20} ->
    //      `{badarg,[{1,10},{2,20}]}` (the constant key cannot follow `'$1'`).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "_")]);
    let constant = int_tuple(&mut heap, &[1, 99]);
    let body = tuple_of(&mut heap, &[at(&atoms, "const"), constant]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{badarg,[{1,10},{2,20}]}"
    );
}

#[test]
fn replace_takes_the_first_matching_clause_per_object() {
    // erl: `[{{1,'_'},[],[{{1,99}}]}, {{'$1','$2'},[],[{{'$1','$2'}}]}]` on
    //      {1,10}/{2,20} -> `{ok:2,[{1,99},{2,20}]}` (clause 1 rewrites object 1,
    //      clause 2 is the identity for object 2).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head1 = tuple_of(&mut heap, &[make_small(1), at(&atoms, "_")]);
    let body1 = construct(&mut heap, &[make_small(1), make_small(99)]);
    let clause1 = clause(&mut heap, head1, &[], &[body1]);

    let head2 = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body2 = construct(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let clause2 = clause(&mut heap, head2, &[], &[body2]);

    let spec = compile(spec_of(&mut heap, &[clause1, clause2]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{1,99},{2,20}]}"
    );
}

#[test]
fn replace_with_a_non_tuple_replacement_is_badarg() {
    // erl: body `'$2'` on {1,10} -> `{badarg,[{1,10}]}` (the value 10 is not a
    //      tuple, so it cannot keep a key).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$2")]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{badarg,[{1,10}]}"
    );
}

#[test]
fn replace_on_an_empty_table_is_ok_with_count_zero() {
    // erl: an empty set -> `{ok:0,[]}` (the body is never evaluated).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:0,[]}"
    );
}

#[test]
fn replace_honours_the_guard_and_leaves_unmatched_objects_alone() {
    // erl: ordered_set {1,10}/{2,20}, body `{{'$1',0}}` guarded by `{'>','$2',15}`
    //      -> `{ok:1,[{1,10},{2,0}]}` (only {2,20} passes the guard).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::OrderedSet);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let guard = tuple_of(
        &mut heap,
        &[at(&atoms, ">"), at(&atoms, "$2"), make_small(15)],
    );
    let body = construct(&mut heap, &[at(&atoms, "$1"), make_small(0)]);
    let spec = compile(spec1(&mut heap, head, &[guard], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line_ordered(&atoms, &mut t, &spec),
        "{ok:1,[{1,10},{2,0}]}"
    );
}

#[test]
fn replace_is_atomic_when_a_later_replacement_is_invalid() {
    // erl: `[{{1,'_'},[],[{{1,99}}]}, {{'$1','$2'},[],[{{9,'$1'}}]}]` on
    //      {1,10}/{2,20} -> `{badarg,[{1,10},{2,20}]}`: clause 1 would have
    //      rewritten object 1, but object 2's replacement `{9,2}` changes the key,
    //      so the whole call is `badarg` and *nothing* was written.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head1 = tuple_of(&mut heap, &[make_small(1), at(&atoms, "_")]);
    let body1 = construct(&mut heap, &[make_small(1), make_small(99)]);
    let clause1 = clause(&mut heap, head1, &[], &[body1]);

    let head2 = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body2 = construct(&mut heap, &[make_small(9), at(&atoms, "$1")]);
    let clause2 = clause(&mut heap, head2, &[], &[body2]);

    let spec = compile(spec_of(&mut heap, &[clause1, clause2]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{badarg,[{1,10},{2,20}]}"
    );
}

#[test]
fn replace_with_keypos_two_keys_on_the_second_element() {
    // erl: table `{keypos, 2}` holding {10,1}/{20,2}, body `{{9,'$2'}}` ->
    //      `{ok:2,[{9,1},{9,2}]}` (element 2 is the key, and it is preserved).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table_keypos(TableKind::Set, 2);
    t.insert(object(&mut heap, 10, 1), &atoms).unwrap();
    t.insert(object(&mut heap, 20, 2), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[make_small(9), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{9,1},{9,2}]}"
    );
}

#[test]
fn replace_on_an_ordered_set_keeps_the_order() {
    // erl: ordered_set {1,10}/{2,20}/{3,30}, head `{3,'_'}` body `{{3,99}}` ->
    //      `{ok:1,[{1,10},{2,20},{3,99}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    let head = tuple_of(&mut heap, &[make_small(3), at(&atoms, "_")]);
    let body = construct(&mut heap, &[make_small(3), make_small(99)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line_ordered(&atoms, &mut t, &spec),
        "{ok:1,[{1,10},{2,20},{3,99}]}"
    );
}

#[test]
fn replace_on_a_duplicate_bag_may_make_every_object_equal() {
    // erl: duplicate_bag {1,10}/{1,20}, head `{1,'_'}` body `{{1,99}}` ->
    //      `{ok:2,[{1,99},{1,99}]}`: a duplicate_bag keeps equal objects, so both
    //      replacements survive (a `set` would collapse them).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::DuplicateBag);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[make_small(1), at(&atoms, "_")]);
    let body = construct(&mut heap, &[make_small(1), make_small(99)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{1,99},{1,99}]}"
    );
}

#[test]
fn replace_on_a_duplicate_bag_can_be_replaced_by_an_equal_object() {
    // erl: duplicate_bag {1,10}/{1,20}, head `{1,20}` body `{{1,10}}` ->
    //      `{ok:1,[{1,10},{1,10}]}`: only the *matched* object changes, and the
    //      result legitimately duplicates the surviving one.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::DuplicateBag);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();

    let head = int_tuple(&mut heap, &[1, 20]);
    let body = construct(&mut heap, &[make_small(1), make_small(10)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:1,[{1,10},{1,10}]}"
    );
}

#[test]
fn replace_on_a_duplicate_bag_with_a_guard_is_counted() {
    // erl: duplicate_bag {1,10}/{1,20}, head `{1,'$2'}` guarded by `{'>','$2',15}`
    //      and the identity body `{{1,'$2'}}` -> `{ok:1,[{1,10},{1,20}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::DuplicateBag);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[make_small(1), at(&atoms, "$2")]);
    let guard = tuple_of(&mut heap, &[at(&atoms, ">"), at(&atoms, "$2"), make_small(15)]);
    let body = construct(&mut heap, &[make_small(1), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[guard], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:1,[{1,10},{1,20}]}"
    );
}

#[test]
fn replace_on_a_duplicate_bag_handles_two_keys_in_one_call() {
    // erl: duplicate_bag {1,10}/{1,20}/{2,30}, body `{{'$1',0}}` guarded by
    //      `{'>','$2',15}` -> `{ok:2,[{1,0},{1,10},{2,0}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::DuplicateBag);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 30), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let guard = tuple_of(&mut heap, &[at(&atoms, ">"), at(&atoms, "$2"), make_small(15)]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), make_small(0)]);
    let spec = compile(spec1(&mut heap, head, &[guard], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:2,[{1,0},{1,10},{2,0}]}"
    );
}

#[test]
fn replace_on_a_duplicate_bag_touches_only_the_matched_object() {
    // erl: duplicate_bag {1,10}/{1,20}/{1,30}, head `{1,'$2'}` guarded by
    //      `{'==','$2',20}` and body `{{1,99}}` -> `{ok:1,[{1,10},{1,30},{1,99}]}`
    //      (the two objects sharing the key that did *not* match are untouched).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::DuplicateBag);
    for value in [10, 20, 30] {
        t.insert(object(&mut heap, 1, value), &atoms).unwrap();
    }

    let head = tuple_of(&mut heap, &[make_small(1), at(&atoms, "$2")]);
    let guard = tuple_of(
        &mut heap,
        &[at(&atoms, "=="), at(&atoms, "$2"), make_small(20)],
    );
    let body = construct(&mut heap, &[make_small(1), make_small(99)]);
    let spec = compile(spec1(&mut heap, head, &[guard], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:1,[{1,10},{1,30},{1,99}]}"
    );
}

#[test]
fn repeated_identical_replacement_on_a_duplicate_bag_is_stable() {
    // erl: duplicate_bag {1,10}/{1,20}/{2,30}; `ets:select_replace(T,
    //      [{{'$1','$2'},[],[{{'$1',99}}]}])` is `3` and leaves
    //      `srt(tab2list) == [{1,99},{1,99},{2,99}]`. Running the *identical* spec
    //      again returns `3` and leaves the table exactly as it was — the rebuild of
    //      each affected key reclaims the objects it replaces, so repeated
    //      replacement is idempotent instead of leaking a generation per call (the
    //      oracle's `ets:info(T, memory)` is stable there too).
    //
    //      The crate's storage-side invariant — the store's live words *and* block
    //      count stay at their warmup floor — is pinned in-crate by
    //      `table::tests::a_duplicate_bag_replacement_reclaims_the_replaced_objects`,
    //      where the store's own accounting is reachable; this test pins the
    //      observable result the oracle can also be asked for.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::DuplicateBag);
    for (key, value) in [(1, 10), (1, 20), (2, 30)] {
        t.insert(object(&mut heap, key, value), &atoms).unwrap();
    }

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), make_small(99)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    let expected = "{ok:3,[{1,99},{1,99},{2,99}]}";
    assert_eq!(replace_line(&atoms, &mut t, &spec), expected);
    let memory = t.memory();
    for round in 1..=100 {
        assert_eq!(
            replace_line(&atoms, &mut t, &spec),
            expected,
            "round {round}: the replacement is not idempotent"
        );
        assert_eq!(
            t.memory(),
            memory,
            "round {round}: `Table::memory` drifted from the warmup floor"
        );
    }
}

#[test]
fn replace_whose_body_raises_is_badarg() {
    // erl: body `{hd,5}` on {1,10}/{2,20} -> `{badarg,[{1,10},{2,20}]}`.
    // `select_replace` does *not* adopt the body-`EXIT` substitution of
    // `select`/`select_count`/`select_delete`: the failed body becomes the atom
    // `EXIT`, which is not a tuple and cannot keep the key, so the call is
    // `badarg` and the table is unchanged.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let body = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{badarg,[{1,10},{2,20}]}"
    );
}

// ---------------------------------------------------------------------------
// Body runtime failures: the atom `EXIT` (`FAIL_TERM`)
// ---------------------------------------------------------------------------

#[test]
fn a_failing_body_expression_yields_the_atom_exit() {
    // erl: `ets:select(T, [{{'_','_'},[],[{hd,5}]}])` on {1,10}/{2,20} ->
    //      `{[EXIT,EXIT],[{1,10},{2,20}]}` — one `EXIT` per matched object.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let body = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    let outcome = select_outcome(&atoms, t.select(&spec, &atoms, &mut output_heap()), false);
    assert_eq!(
        composed(&outcome, &sorted_table(&atoms, &t)),
        "{[EXIT,EXIT],[{1,10},{2,20}]}"
    );
}

#[test]
fn a_later_body_expression_supplies_the_clause_value() {
    // erl: body `[{hd,5}, 7]` -> `{[7,7],[{1,10},{2,20}]}`: the failed first
    //      expression is replaced by `EXIT` and the clause's value is the value of
    //      its LAST expression.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let failing = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[failing, make_small(7)]), &atoms).unwrap();

    let outcome = select_outcome(&atoms, t.select(&spec, &atoms, &mut output_heap()), false);
    assert_eq!(
        composed(&outcome, &sorted_table(&atoms, &t)),
        "{[7,7],[{1,10},{2,20}]}"
    );
}

#[test]
fn a_bound_variable_after_a_failed_body_expression_yields_the_binding() {
    // erl: ordered_set, head `{'$1','_'}`, body `[{hd,5}, '$1']` ->
    //      `{[1,2],[{1,10},{2,20}]}`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::OrderedSet);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "_")]);
    let failing = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec =
        compile(spec1(&mut heap, head, &[], &[failing, at(&atoms, "$1")]), &atoms).unwrap();

    let outcome = select_outcome(&atoms, t.select(&spec, &atoms, &mut output_heap()), true);
    assert_eq!(
        composed(&outcome, &inorder_table(&atoms, &t)),
        "{[1,2],[{1,10},{2,20}]}"
    );
}

#[test]
fn two_failing_body_expressions_leave_the_last_exit() {
    // erl: body `[{hd,5}, {element,9,'$1'}]` on {1,10} ->
    //      `{[EXIT],[{1,10}]}` (both failures become `EXIT`; the last one is the
    //      clause's value).
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "_")]);
    let first = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let second = tuple_of(
        &mut heap,
        &[at(&atoms, "element"), make_small(9), at(&atoms, "$1")],
    );
    let spec = compile(spec1(&mut heap, head, &[], &[first, second]), &atoms).unwrap();

    let outcome = select_outcome(&atoms, t.select(&spec, &atoms, &mut output_heap()), false);
    assert_eq!(
        composed(&outcome, &sorted_table(&atoms, &t)),
        "{[EXIT],[{1,10}]}"
    );
}

#[test]
fn select_count_sees_exit_and_reports_zero() {
    // erl: `ets:select_count(T, [{{'_','_'},[],[{hd,5}]}])` on {1,10}/{2,20} ->
    //      `{0,[{1,10},{2,20}]}`: `EXIT` is not `true`, so nothing counts.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let body = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    let outcome = count_outcome(t.select_count(&spec, &atoms, &mut output_heap()));
    assert_eq!(
        composed(&outcome, &sorted_table(&atoms, &t)),
        "{0,[{1,10},{2,20}]}"
    );
}

#[test]
fn select_delete_sees_exit_and_deletes_nothing() {
    // erl: `ets:select_delete(T, [{{'_','_'},[],[{hd,5}]}])` on {1,10}/{2,20} ->
    //      `{0,[{1,10},{2,20}]}`: `EXIT` never satisfies the deletion test.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let body = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    let outcome = count_outcome(t.select_delete(&spec, &atoms, &mut output_heap()));
    assert_eq!(
        composed(&outcome, &sorted_table(&atoms, &t)),
        "{0,[{1,10},{2,20}]}"
    );
}

#[test]
fn a_failing_guard_still_skips_the_clause() {
    // erl: guard `{hd,5}` on {1,10}/{2,20} -> `{[],[{1,10},{2,20}]}`.
    // Guards do **not** compile under `matchCatch`, so they keep their original
    // meaning: a raising guard makes the clause fail instead of yielding `EXIT`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let guard = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[guard], &[at(&atoms, "$_")]), &atoms).unwrap();

    let outcome = select_outcome(&atoms, t.select(&spec, &atoms, &mut output_heap()), false);
    assert_eq!(
        composed(&outcome, &sorted_table(&atoms, &t)),
        "{[],[{1,10},{2,20}]}"
    );
}

#[test]
fn a_body_failure_is_badarg_when_exit_is_not_interned() {
    // The `EXIT` substitution resolves the atom by name. The oracle's `FAIL_TERM`
    // is a static atom that always exists, so an `AtomTable` that has not
    // interned `EXIT` is a capability gap here: the failure stays `badarg` rather
    // than substituting a different atom (never a wrong result).
    let mut atoms = AtomTable::new();
    for name in ["_", "hd"] {
        atoms.intern(name);
    }
    assert!(atoms.lookup("EXIT").is_none());

    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let body = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    let err = t
        .select(&spec, &atoms, &mut output_heap())
        .expect_err("an uninterned `EXIT` cannot be substituted");
    assert!(
        matches!(err, BeamError::BadArg),
        "expected badarg, got: {err:?}"
    );
}

#[test]
fn only_badarg_is_masked_by_exit_never_a_typed_capability_gap() {
    // Only the oracle's own `badarg` becomes `FAIL_TERM`; any other failure keeps
    // propagating (workspace `CONTEXT.md` §3: a typed capability gap must never be
    // masked). `expr::eval` has no non-`badarg` failure today, so this pins the
    // rule with the one failure a body can really produce here: a heap too small
    // for the constructed tuple, which surfaces as the typed `out_of_memory`
    // allocation error — *not* `EXIT`.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    // One word of heap cannot hold the 3-word constructed tuple.
    let mut tiny = beam_alloc::Heap::new(1).expect("a one-word heap is allocatable");
    let select_err = t
        .select(&spec, &atoms, &mut tiny)
        .expect_err("the construction must fail");
    assert!(
        matches!(select_err, BeamError::Alloc(_)),
        "expected the allocation error, got: {select_err:?}"
    );

    let mut tiny = beam_alloc::Heap::new(1).expect("a one-word heap is allocatable");
    let replace_err = t
        .select_replace(&spec, &atoms, &mut tiny)
        .expect_err("the construction must fail");
    assert!(
        matches!(replace_err, BeamError::Alloc(_)),
        "expected the allocation error, got: {replace_err:?}"
    );
}

// ---------------------------------------------------------------------------
// The oracle's *static* body check (see the module docs)
//
// `erl` raises `badarg` for a body that is not a one-expression list of `'$_'`,
// `{{ … }}` or `{const, {…}}`, and requires the constructed key to be
// *syntactically* the head's key — even when the spec matches nothing. `beam_ets`
// adopts exactly that rule, and it runs *before* any traversal, so every rejection
// below is asserted together with the unchanged table (via `replace_line`, which
// renders the table after the call).
// ---------------------------------------------------------------------------

#[test]
fn static_check_multi_expression_body_is_badarg() {
    // erl: body `[{hd,5}, {{'$1','$2'}}]` on {1,10}/{2,20} is `badarg` (the C
    //      requires a ONE-expression body — `db_match_keeps_key`).
    // The second expression would rebuild the matched tuple and keep the key, but
    // the body's *shape* is rejected before anything is traversed.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let failing = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let construct_identity = construct(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(
        spec1(&mut heap, head, &[], &[failing, construct_identity]),
        &atoms,
    )
    .unwrap();

    assert_eq!(replace_line(&atoms, &mut t, &spec), "{badarg,[{1,10},{2,20}]}");
}

#[test]
fn static_check_multi_expression_body_on_a_never_matching_spec_is_badarg() {
    // erl: `[{{99,'_'},[],[{hd,5},{{99,1}}]}]` on {1,10} is `badarg` — the oracle
    //      validates the body's shape *before* traversing, so an empty match set
    //      does not help.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[make_small(99), at(&atoms, "_")]);
    let failing = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let constructed = construct(&mut heap, &[make_small(99), make_small(1)]);
    let spec = compile(spec1(&mut heap, head, &[], &[failing, constructed]), &atoms).unwrap();

    assert_eq!(replace_line(&atoms, &mut t, &spec), "{badarg,[{1,10}]}");
}

#[test]
fn static_check_constant_body_with_a_wildcard_head_key_is_badarg() {
    // erl: `[{{'_','_'},[],[{const,{1,10}}]}]` on {1,10} is `badarg` — a `'_'` head
    //      key cannot be *syntactically* tied to the constant body key.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let constant = int_tuple(&mut heap, &[1, 10]);
    let body = tuple_of(&mut heap, &[at(&atoms, "const"), constant]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(replace_line(&atoms, &mut t, &spec), "{badarg,[{1,10}]}");
}

#[test]
fn an_unbound_body_variable_is_badarg_like_the_oracle() {
    // erl: `[{{1,'_'},[],[{{'$1',99}}]}]` on {1,10} -> `{badarg,[{1,10}]}`: the
    //      head `{1,'_'}` binds no `'$1'`, so **the spec itself** is rejected
    //      (the C `dmc_variable` error) before `select_replace` looks at the table
    //      — both implementations agree, and the table is untouched.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[make_small(1), at(&atoms, "_")]);
    let body = construct(&mut heap, &[at(&atoms, "$1"), make_small(99)]);
    let spec = spec1(&mut heap, head, &[], &[body]);

    assert!(
        matches!(compile(spec, &atoms), Err(BeamError::BadArg)),
        "an unbound body variable must not compile"
    );
    assert_eq!(sorted_table(&atoms, &t), "[{1,10}]", "the table is untouched");
}

#[test]
fn static_check_rejected_body_on_a_never_matching_spec_is_badarg() {
    // erl: `[{{99,'_'},[],[{hd,5}]}]` on {1,10} is `badarg` (the body `{hd,5}` is
    //      not a construction, whatever it would evaluate to) — the check runs
    //      before the traversal, so the empty match set does not help.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[make_small(99), at(&atoms, "_")]);
    let body = tuple_of(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(replace_line(&atoms, &mut t, &spec), "{badarg,[{1,10}]}");
}

#[test]
fn static_check_rejected_body_on_an_empty_table_is_badarg() {
    // erl: `[{{'$1','$2'},[],[ '$2']}]` on an EMPTY set is `badarg` (the body is a
    //      bare variable, not a construction) — the shape is validated before the
    //      (empty) traversal, so an empty table does not help either.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);

    let head = tuple_of(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$2")]), &atoms).unwrap();

    assert_eq!(replace_line(&atoms, &mut t, &spec), "{badarg,[]}");
}

#[test]
fn the_element_keypos_whole_object_construct_form_agrees_with_the_oracle() {
    // erl: `[{{'_','_'},[],[{{{element,1,'$_'},99}}]}]` on {1,10} ->
    //      `{ok:1,[{1,99}]}` — `{element, KeyPos, '$_'}` is one of the forms the
    //      oracle's static check allows, and `beam_ets`'s static check allows it too.
    let atoms = atoms();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let head = tuple_of(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let element = tuple_of(
        &mut heap,
        &[at(&atoms, "element"), make_small(1), at(&atoms, "$_")],
    );
    let body = construct(&mut heap, &[element, make_small(99)]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    assert_eq!(
        replace_line(&atoms, &mut t, &spec),
        "{ok:1,[{1,99}]}"
    );
}
