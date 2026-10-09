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

//! End-to-end tests for the table-independent match-spec runner
//! (`ets:match_spec_run/2`) over raw match-spec clause lists.
//!
//! Every expectation below was derived by running the same spec against the system
//! `erl` (OTP 29 / erts-17.1, `erl -noshell -eval '…' -s init stop`); the probe
//! results are quoted next to the assertion. The oracle's `ets:match_spec_run/2`
//! takes a spec **already compiled** by `ets:match_spec_compile/1`, so the probes
//! wrap each raw clause list in `ets:match_spec_compile/1`; this crate has no
//! compiled-spec term and [`match_spec_run`] compiles the raw list itself.
//!
//! Note: `'$$'` written literally in Erlang source is the one-character atom `'$'`,
//! so the all-bindings probes use `list_to_atom([36,36])`.

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_ets::prelude::*;
use beam_term::prelude::*;

/// The atoms the specs below reference. `true`/`false` must be interned: a body
/// whose value is an internal boolean is rendered as the atom it names.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in [
        "_", "$_", "$$", "$0", "$1", "$2", "$3", "$5", "$x", "true", "false", "foo", "a", "b", "c",
        "d", "x", "y", "ok", ">", "<", "=:=", "=/=", "==", "=<", ">=", "andalso", "orelse",
        "or", "not", "const", "hd", "tl", "length", "abs", "size", "element", "self", "is_integer",
        "is_float", "is_number", "is_atom", "is_list", "is_tuple", "is_binary", "is_bitstring",
        "is_map", "/", "div",
    ] {
        atoms.intern(name);
    }
    atoms
}

fn at(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).expect("interned by `atoms`"))
}

/// [`atoms`] plus the `EXIT` atom a failed match-spec *body* expression is replaced
/// by (`exec::exit_term` — the C `FAIL_TERM`). `EXIT` is kept out of the shared
/// [`atoms`] because the crate reports a typed `badarg` capability gap when the
/// caller's table has not interned it; a real runtime always interns it
/// (`beam_rt::boot::CORE_ATOMS`).
fn atoms_with_exit() -> AtomTable {
    let mut atoms = atoms();
    let _ = atoms.intern("EXIT");
    atoms
}

fn tup(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    heap.tuple(elements).expect("test heap has room")
}

fn list(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for element in elements.iter().rev() {
        acc = heap.cons(*element, acc).expect("test heap has room");
    }
    acc
}

/// `[ {Head, Guards, Body} ]` for a single clause.
fn spec1(heap: &mut Heap, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
    let guard_list = list(heap, guards);
    let body_list = list(heap, body);
    let clause = tup(heap, &[head, guard_list, body_list]);
    list(heap, &[clause])
}

/// A match-spec call term `{Name, Args…}` (the guard/body function-call form).
fn call(heap: &mut Heap, name: Eterm, args: &[Eterm]) -> Eterm {
    let mut elements = Vec::with_capacity(args.len() + 1);
    elements.push(name);
    elements.extend_from_slice(args);
    tup(heap, &elements)
}

/// Assert two term sequences are structurally equal, element by element.
fn assert_terms_eq(got: &[Eterm], want: &[Eterm], atoms: &AtomTable) {
    assert_eq!(got.len(), want.len(), "result length");
    for (index, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(eq(*g, *w, atoms), "result {index} differs");
    }
}

/// `[{A,B}, …]` — a fun2ms input list of 2-tuples of small ints.
///
/// (A builder rather than an inline `list(&mut heap, &[tup(&mut heap, …), …])`,
/// which cannot borrow the heap twice at once.)
fn pair_list(heap: &mut Heap, values: &[(i64, i64)]) -> Eterm {
    let mut elements = Vec::with_capacity(values.len());
    for &(a, b) in values {
        elements.push(tup(heap, &[make_small(a), make_small(b)]));
    }
    list(heap, &elements)
}

/// `[{A,{B,C}}, …]` — a fun2ms input list of nested-tuple elements.
fn nested_pair_list(heap: &mut Heap, values: &[(i64, i64, i64)]) -> Eterm {
    let mut elements = Vec::with_capacity(values.len());
    for &(a, b, c) in values {
        let inner = tup(heap, &[make_small(b), make_small(c)]);
        elements.push(tup(heap, &[make_small(a), inner]));
    }
    list(heap, &elements)
}

/// `[{K,[V1,V2]}, …]` — a fun2ms input list whose element holds a 2-element list.
fn list_pair_list(heap: &mut Heap, values: &[(i64, i64, i64)]) -> Eterm {
    let mut elements = Vec::with_capacity(values.len());
    for &(k, v1, v2) in values {
        let tail = list(heap, &[make_small(v1), make_small(v2)]);
        elements.push(tup(heap, &[make_small(k), tail]));
    }
    list(heap, &elements)
}

// ---------------------------------------------------------------------------
// Head matching
// ---------------------------------------------------------------------------

#[test]
fn head_matches_the_element_itself_whatever_its_shape() {
    // erl: `ets:match_spec_run([foo,1,{1,a}],
    //      ets:match_spec_compile([{'$1',[],[{{'$1'}}]}]))` -> `[{foo},{1},{{1,a}}]`.
    // The head matches the element itself — an atom, an integer, a tuple — not
    // only a tuple.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let inner = tup(&mut heap, &[at(&atoms, "$1")]);
    let body_element = tup(&mut heap, &[inner]);
    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[body_element]);

    let tuple_element = tup(&mut heap, &[make_small(1), at(&atoms, "a")]);
    let input = list(
        &mut heap,
        &[at(&atoms, "foo"), make_small(1), tuple_element],
    );

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");

    let foo = tup(&mut heap, &[at(&atoms, "foo")]);
    let one = tup(&mut heap, &[make_small(1)]);
    let tuple_in_tuple = tuple_element;
    let wrapped = tup(&mut heap, &[tuple_in_tuple]);
    let want = [foo, one, wrapped];
    assert_terms_eq(&got, &want, &atoms);
}

#[test]
fn repeated_head_variable_enforces_equality() {
    // erl: `ets:match_spec_run([{1,1},{1,2}],
    //      ets:match_spec_compile([{{'$1','$1'},[],['$1']}]))` -> `[1]`
    // (`{1,2}` has unequal elements, so its clause does not match).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$1")]);

    let equal = tup(&mut heap, &[make_small(1), make_small(1)]);
    let unequal = tup(&mut heap, &[make_small(1), make_small(2)]);
    let input = list(&mut heap, &[equal, unequal]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(1)], &atoms);
}

#[test]
fn head_dollar_underscore_never_matches_a_non_atom() {
    // erl: `ets:match_spec_run([foo,1,{1,a}],
    //      ets:match_spec_compile([{'$_',[],[true]}]))` -> `[]`. `'$_'` is only the
    // whole object in a BODY; in a HEAD it is not a wildcard, so it never matches a
    // non-atom (nor a tuple).
    //
    // KNOWN DIVERGENCE (not asserted — the head decoder is owned by the match-spec
    // `pattern` module, not by this runner): the oracle additionally matches the
    // *literal* atom `'$_'` there (`ets:match_spec_run(['$_'], …)` -> `[true]`, and
    // `list_to_atom([36,36])` likewise), because the C `dmc_one_term` emits `matchEq`
    // for any atom that is not `'_'`/`'$N'` (`erl_db_util.c:4117-4121`). This crate's
    // `pattern::decode` returns `Pattern::Fail` for `'$_'`/`'$$'`, so it never matches.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    for head in ["$_", "$$"] {
        let spec = spec1(&mut heap, at(&atoms, head), &[], &[at(&atoms, "true")]);
        let tuple_element = tup(&mut heap, &[make_small(1), at(&atoms, "a")]);
        let non_atoms = list(
            &mut heap,
            &[at(&atoms, "foo"), make_small(1), tuple_element],
        );
        let got = match_spec_run(non_atoms, spec, &atoms, &mut heap).expect("run");
        assert!(
            got.is_empty(),
            "`{head}` in a head must not match a non-atom"
        );
    }
}
// ---------------------------------------------------------------------------
// Clauses, order, bodies
// ---------------------------------------------------------------------------

#[test]
fn the_first_matching_clause_wins() {
    // erl: `ets:match_spec_run([x,y],
    //      ets:match_spec_compile([{'$1',[],[a]},{'$1',[],[b]}]))` -> `[a,a]`
    // (both clauses match every element; the FIRST one's body is used).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let body_a = list(&mut heap, &[at(&atoms, "a")]);
    let first = tup(&mut heap, &[at(&atoms, "$1"), NIL, body_a]);
    let body_b = list(&mut heap, &[at(&atoms, "b")]);
    let second = tup(&mut heap, &[at(&atoms, "$1"), NIL, body_b]);
    let spec = list(&mut heap, &[first, second]);
    let input = list(&mut heap, &[at(&atoms, "x"), at(&atoms, "y")]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[at(&atoms, "a"), at(&atoms, "a")], &atoms);
}

#[test]
fn order_is_preserved_and_unmatched_elements_are_dropped() {
    // erl: `ets:match_spec_run([{1,2},{3,4}],
    //      ets:match_spec_compile([{{'$1','$2'},[],[{{'$2','$1'}}]}]))`
    //      -> `[{2,1},{4,3}]` (input order preserved, unlike a table's select).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let swapped = tup(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$1")]);
    let body_swap = tup(&mut heap, &[swapped]);
    let spec = spec1(&mut heap, head, &[], &[body_swap]);

    let first = tup(&mut heap, &[make_small(1), make_small(2)]);
    let second = tup(&mut heap, &[make_small(3), make_small(4)]);
    let input = list(&mut heap, &[first, second]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");

    let swapped_first = tup(&mut heap, &[make_small(2), make_small(1)]);
    let swapped_second = tup(&mut heap, &[make_small(4), make_small(3)]);
    assert_terms_eq(&got, &[swapped_first, swapped_second], &atoms);

    // erl: `[{{'$1','$1'},[],['$1']}]` over `[{1,1},{2,3},{4,4}]` -> `[1,4]`
    // (`{2,3}` is dropped, the survivors keep their relative order).
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$1")]);
    let equal_a = tup(&mut heap, &[make_small(1), make_small(1)]);
    let unequal = tup(&mut heap, &[make_small(2), make_small(3)]);
    let equal_b = tup(&mut heap, &[make_small(4), make_small(4)]);
    let input = list(&mut heap, &[equal_a, unequal, equal_b]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(1), make_small(4)], &atoms);
}

#[test]
fn body_whole_object_is_the_element() {
    // erl: `ets:match_spec_run([a,b], ets:match_spec_compile([{'$1',[],['$_']}]))`
    //      -> `[a,b]` (`'$_'` in a BODY is the whole element).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[at(&atoms, "$_")]);
    let input = list(&mut heap, &[at(&atoms, "a"), at(&atoms, "b")]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[at(&atoms, "a"), at(&atoms, "b")], &atoms);
}

#[test]
fn body_all_bindings_lists_every_head_variable() {
    // erl: `ets:match_spec_run([{1,2},{3,4}],
    //      ets:match_spec_compile([{{'$1','$2'},[],['$$']}]))` -> `[[1,2],[3,4]]`
    // (the all-bindings atom `'$$'`, built with `list_to_atom([36,36])`).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$$")]);

    let first = tup(&mut heap, &[make_small(1), make_small(2)]);
    let second = tup(&mut heap, &[make_small(3), make_small(4)]);
    let input = list(&mut heap, &[first, second]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");

    let bindings_first = list(&mut heap, &[make_small(1), make_small(2)]);
    let bindings_second = list(&mut heap, &[make_small(3), make_small(4)]);
    assert_terms_eq(&got, &[bindings_first, bindings_second], &atoms);
}

#[test]
fn guards_filter_the_elements() {
    // erl: `ets:match_spec_run([1,2,3],
    //      ets:match_spec_compile([{'$1',[{'>','$1',1}],['$1']}]))` -> `[2,3]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let guard = tup(
        &mut heap,
        &[at(&atoms, ">"), at(&atoms, "$1"), make_small(1)],
    );
    let spec = spec1(&mut heap, at(&atoms, "$1"), &[guard], &[at(&atoms, "$1")]);
    let input = list(&mut heap, &[make_small(1), make_small(2), make_small(3)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(2), make_small(3)], &atoms);
}

#[test]
fn a_literal_body_is_repeated_per_match() {
    // erl: `ets:match_spec_run([foo,1], ets:match_spec_compile([{'$1',[],[ok]}]))`
    //      -> `[ok,ok]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[at(&atoms, "ok")]);
    let input = list(&mut heap, &[at(&atoms, "foo"), make_small(1)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[at(&atoms, "ok"), at(&atoms, "ok")], &atoms);
}

// ---------------------------------------------------------------------------
// Empty input, no match, malformed input
// ---------------------------------------------------------------------------

#[test]
fn empty_list_yields_no_results() {
    // erl: `ets:match_spec_run([], ets:match_spec_compile([{'$1',[],[{{'$1'}}]}]))`
    //      -> `[]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let inner = tup(&mut heap, &[at(&atoms, "$1")]);
    let body_element = tup(&mut heap, &[inner]);
    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[body_element]);

    let got = match_spec_run(NIL, spec, &atoms, &mut heap).expect("run");
    assert!(got.is_empty());
}

#[test]
fn no_matching_clause_drops_every_element() {
    // erl: `ets:match_spec_run([{1,2}],
    //      ets:match_spec_compile([{{'$1','$1'},[],['$1']}]))` -> `[]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$1")]);

    let element = tup(&mut heap, &[make_small(1), make_small(2)]);
    let input = list(&mut heap, &[element]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert!(got.is_empty());
}

#[test]
fn improper_list_and_non_list_are_badarg() {
    // erl: `ets:match_spec_run([a|b], Compiled)` -> `error:badarg`;
    //      `ets:match_spec_run(foo, Compiled)` -> `error:badarg`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[at(&atoms, "$1")]);

    let improper = heap.cons(at(&atoms, "a"), at(&atoms, "b")).expect("heap");
    assert!(matches!(
        match_spec_run(improper, spec, &atoms, &mut heap),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        match_spec_run(at(&atoms, "foo"), spec, &atoms, &mut heap),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn malformed_spec_is_badarg() {
    // erl: `ets:match_spec_run([foo], 1)` -> `error:badarg`; a spec clause that is
    // not a 3-tuple is likewise rejected by `ets:match_spec_compile/1`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let input = list(&mut heap, &[at(&atoms, "foo")]);

    // A spec that is not a list.
    assert!(matches!(
        match_spec_run(input, make_small(1), &atoms, &mut heap),
        Err(BeamError::BadArg)
    ));

    // A clause that is not a `{Head, Guards, Body}` 3-tuple.
    let two = tup(&mut heap, &[at(&atoms, "$1"), NIL]);
    let bad_spec = list(&mut heap, &[two]);
    assert!(matches!(
        match_spec_run(input, bad_spec, &atoms, &mut heap),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// `ets:fun2ms/1`-shaped specs
// ---------------------------------------------------------------------------
//
// `ets:fun2ms/1` (the `ms_transform` parse transform) compiles a fun into a
// one-clause match spec `[{Head, Guards, Body}]` whose head binds `'$1'`, `'$2'`, …
// left to right and whose body is the fun's result expression. Every spec below is
// the **verbatim** term the oracle emits for the fun quoted beside it — captured
// from a compiled module (`-compile({parse_transform, ms_transform})`, see below),
// because calling `ets:fun2ms/1` on a plain (untransformed) fun is `badarg`:
//
// ```erlang
// -module(f2m).
// -compile({parse_transform, ms_transform}).
// -include_lib("stdlib/include/ms_transform.hrl").
// main() -> io:format("~p~n", [ets:fun2ms(fun({M,N}) when M > 2, N < 9 -> {M,N} end)]).
// ```
//
// This pins the exact *syntactic shapes* the transform produces — positional
// `'$N'` bindings, a guard list, a `{{ … }}`/`[ … ]` body, `'_'` for an ignored
// head position — which the hand-written specs elsewhere in this suite do not.

#[test]
fn fun2ms_guard_only_spec_returns_the_bound_variable() {
    // erl: `ets:fun2ms(fun({M,N}) when M > 2 -> M end)`
    //      -> `[{{'$1','$2'},[{'>','$1',2}],['$1']}]`;
    //      run over `[{1,5},{3,4},{5,10},{4,8}]` -> `[3,5,4]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let guard = tup(
        &mut heap,
        &[at(&atoms, ">"), at(&atoms, "$1"), make_small(2)],
    );
    let spec = spec1(&mut heap, head, &[guard], &[at(&atoms, "$1")]);

    let input = pair_list(&mut heap, &[(1, 5), (3, 4), (5, 10), (4, 8)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(3), make_small(5), make_small(4)], &atoms);
}

#[test]
fn fun2ms_guard_conjunction_spec_returns_a_tuple_body() {
    // erl: `ets:fun2ms(fun({M,N}) when M > 2, N < 9 -> {M,N} end)`
    //      -> `[{{'$1','$2'},[{'>','$1',2},{'<','$2',9}],[{{'$1','$2'}}]}]`
    //      (a guard *conjunction* is one guard list, in source order);
    //      run over `[{1,5},{3,4},{5,10},{4,8}]` -> `[{3,4},{4,8}]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let greater = tup(
        &mut heap,
        &[at(&atoms, ">"), at(&atoms, "$1"), make_small(2)],
    );
    let less = tup(
        &mut heap,
        &[at(&atoms, "<"), at(&atoms, "$2"), make_small(9)],
    );
    // The body element is `{{'$1','$2'}}`: a 1-tuple whose element is the
    // construction tuple (a bare `{'$1','$2'}` body element is a *call* → badarg).
    let construction = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body_element = tup(&mut heap, &[construction]);
    let spec = spec1(&mut heap, head, &[greater, less], &[body_element]);

    let input = pair_list(&mut heap, &[(1, 5), (3, 4), (5, 10), (4, 8)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    let want = [
        tup(&mut heap, &[make_small(3), make_small(4)]),
        tup(&mut heap, &[make_small(4), make_small(8)]),
    ];
    assert_terms_eq(&got, &want, &atoms);
}

#[test]
fn fun2ms_identity_spec_matches_any_element() {
    // erl: `ets:fun2ms(fun(X) -> X end)` -> `[{'$1',[],['$1']}]` — the head is a
    // bare `'$1'`, so it matches an element of *any* shape, not only a tuple.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[at(&atoms, "$1")]);
    let mut elements = Vec::new();
    elements.push(tup(&mut heap, &[make_small(1), make_small(5)]));
    elements.push(at(&atoms, "foo"));
    elements.push(make_small(7));
    let input = list(&mut heap, &elements);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    let want = [
        tup(&mut heap, &[make_small(1), make_small(5)]),
        at(&atoms, "foo"),
        make_small(7),
    ];
    assert_terms_eq(&got, &want, &atoms);
}

#[test]
fn fun2ms_nested_tuple_head_and_tuple_body() {
    // erl: `ets:fun2ms(fun({A,{B,C}}) -> {C,B,A} end)`
    //      -> `[{{'$1',{'$2','$3'}},[],[{{'$3','$2','$1'}}]}]`;
    //      run over `[{2,{5,6}},{5,{10,11}},{8,{15,16}}]`
    //      -> `[{6,5,2},{11,10,5},{16,15,8}]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let inner_head = tup(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$3")]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), inner_head]);
    let inner_body = tup(
        &mut heap,
        &[at(&atoms, "$3"), at(&atoms, "$2"), at(&atoms, "$1")],
    );
    let body_element = tup(&mut heap, &[inner_body]);
    let spec = spec1(&mut heap, head, &[], &[body_element]);

    let input = nested_pair_list(&mut heap, &[(2, 5, 6), (5, 10, 11), (8, 15, 16)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    let want = [
        tup(&mut heap, &[make_small(6), make_small(5), make_small(2)]),
        tup(&mut heap, &[make_small(11), make_small(10), make_small(5)]),
        tup(&mut heap, &[make_small(16), make_small(15), make_small(8)]),
    ];
    assert_terms_eq(&got, &want, &atoms);
}

#[test]
fn fun2ms_nested_wildcard_and_type_test_guard() {
    // erl: `ets:fun2ms(fun({{_,X}}) when is_integer(X) -> X end)`
    //      -> `[{{{'_','$1'}},[{is_integer,'$1'}],['$1']}]` — the ignored position
    //      is `'_'`, the bound one `'$1'` (positional numbering skips nothing);
    //      run over `[{{a,3}},{{b,c}},{{d,7}}]` -> `[3,7]` (the type test rejects
    //      `{b,c}` by *not matching*, not by raising).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let inner_head = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "$1")]);
    let head = tup(&mut heap, &[inner_head]);
    let guard = tup(&mut heap, &[at(&atoms, "is_integer"), at(&atoms, "$1")]);
    let spec = spec1(&mut heap, head, &[guard], &[at(&atoms, "$1")]);

    let nested_a = tup(&mut heap, &[at(&atoms, "a"), make_small(3)]);
    let nested_b = tup(&mut heap, &[at(&atoms, "b"), at(&atoms, "c")]);
    let nested_d = tup(&mut heap, &[at(&atoms, "d"), make_small(7)]);
    let element_a = tup(&mut heap, &[nested_a]);
    let element_b = tup(&mut heap, &[nested_b]);
    let element_d = tup(&mut heap, &[nested_d]);
    let input = list(&mut heap, &[element_a, element_b, element_d]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(3), make_small(7)], &atoms);
}

#[test]
fn fun2ms_list_head_pattern_and_list_body() {
    // erl: `ets:fun2ms(fun({K,[V1,V2]}) -> [K,V1] end)`
    //      -> `[{{'$1',['$2','$3']},[],[['$1','$2']]}]` — the head's list is a
    //      proper-list pattern of exactly two elements (so `{1,[2]}` cannot match),
    //      and the body is a **list** construction;
    //      run over `[{1,[2,3]},{2,[3,4]},{4,[5,6]}]` -> `[[1,2],[2,3],[4,5]]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let list_head = list(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$3")]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), list_head]);
    let body_element = list(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = spec1(&mut heap, head, &[], &[body_element]);

    let input = list_pair_list(&mut heap, &[(1, 2, 3), (2, 3, 4), (4, 5, 6)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    let want = [
        list(&mut heap, &[make_small(1), make_small(2)]),
        list(&mut heap, &[make_small(2), make_small(3)]),
        list(&mut heap, &[make_small(4), make_small(5)]),
    ];
    assert_terms_eq(&got, &want, &atoms);
}

#[test]
fn fun2ms_multi_element_body_yields_its_last_value() {
    // The transform can emit more than one body expression when the fun body is a
    // match/block; the clause's value is the value of the **last** one. This is the
    // shape `[{{'$1','$2'},[],[ok,'$2']}]`:
    // erl: `ets:match_spec_run([{1,5},{3,4},{4,8},{5,10}], ets:match_spec_compile(
    //      [{{'$1','$2'},[],[ok,'$2']}]))` -> `[5,4,8,10]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "ok"), at(&atoms, "$2")]);

    let input = pair_list(&mut heap, &[(1, 5), (3, 4), (4, 8), (5, 10)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(
        &got,
        &[
            make_small(5),
            make_small(4),
            make_small(8),
            make_small(10),
        ],
        &atoms,
    );
}

#[test]
fn fun2ms_all_bindings_body_lists_every_head_variable() {
    // The all-bindings body atom `'$$'` (built in Erlang with `list_to_atom([36,36])`
    // — writing `'$$'` in source is the one-character atom `'$'`):
    // erl: `ets:match_spec_run([{1,5},{3,4}], ets:match_spec_compile(
    //      [{{'$1','$2'},[],[list_to_atom([36,36])]}]))` -> `[[1,5],[3,4]]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$$")]);

    let input = pair_list(&mut heap, &[(1, 5), (3, 4)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    let want = [
        list(&mut heap, &[make_small(1), make_small(5)]),
        list(&mut heap, &[make_small(3), make_small(4)]),
    ];
    assert_terms_eq(&got, &want, &atoms);
}

#[test]
fn fun2ms_andalso_guard_is_a_single_guard_expression() {
    // A guard *conjunction* written with `andalso` is one nested guard term
    // (`'andalso'` is the operator's own atom, not the Erlang `andalso` keyword):
    // erl: `ets:match_spec_run([{1,5},{3,4},{4,8},{5,10}], ets:match_spec_compile(
    //      [{{'$1','$2'},[{'andalso',{'>','$1',1},{'<','$2',10}}],['$1']}]))`
    //      -> `[3,4]` (`{1,5}` fails `'>'`, `{5,10}` fails `'<'`).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let greater = tup(
        &mut heap,
        &[at(&atoms, ">"), at(&atoms, "$1"), make_small(1)],
    );
    let less = tup(
        &mut heap,
        &[at(&atoms, "<"), at(&atoms, "$2"), make_small(10)],
    );
    let guard = tup(&mut heap, &[at(&atoms, "andalso"), greater, less]);
    let spec = spec1(&mut heap, head, &[guard], &[at(&atoms, "$1")]);

    let input = pair_list(&mut heap, &[(1, 5), (3, 4), (4, 8), (5, 10)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(3), make_small(4)], &atoms);
}

#[test]
fn fun2ms_whole_object_body_returns_each_matching_element() {
    // erl: `ets:match_spec_run([{1,5},{3,4},{4,8},{5,10}], ets:match_spec_compile(
    //      [{{'$1','$2'},[],['$_']}]))` -> the same four elements, in order.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$_")]);

    let input = pair_list(&mut heap, &[(1, 5), (3, 4), (4, 8), (5, 10)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    let want = [
        tup(&mut heap, &[make_small(1), make_small(5)]),
        tup(&mut heap, &[make_small(3), make_small(4)]),
        tup(&mut heap, &[make_small(4), make_small(8)]),
        tup(&mut heap, &[make_small(5), make_small(10)]),
    ];
    assert_terms_eq(&got, &want, &atoms);
}

// ===========================================================================
// Guard/body construct completeness over `match_spec_run/2`
//
// Same oracle and same convention as the rest of this file: every expectation is
// the system `erl`'s (OTP 29 / erts-17.1), probed by wrapping the raw clause list
// in `ets:match_spec_compile/1` (the oracle's `ets:match_spec_run/2` requires a
// compiled spec), with the probe result quoted beside the assertion.
// ===========================================================================

/// Compile a one-clause `{Head, Guards, Body}` spec and run it over `input`.
///
/// (A helper rather than a `match_spec_run(input, spec1(&mut heap, head, …), …)`
/// call, which cannot borrow the heap twice at once.)
fn run_one(
    input: Eterm,
    head: Eterm,
    guards: &[Eterm],
    body: &[Eterm],
    atoms: &AtomTable,
    heap: &mut Heap,
) -> beam_common::Result<Vec<Eterm>> {
    let spec = spec1(heap, head, guards, body);
    match_spec_run(input, spec, atoms, heap)
}

/// A float term on the test heap (the exact-versus-arithmetic comparison probe).
fn float(heap: &mut Heap, value: f64) -> Eterm {
    // SAFETY: `alloc_words(2)` hands back the two exclusively-owned, writable words a
    // float occupies (`[HEADER_FLONUM, bits]`); `write_float` writes both before the
    // term escapes, and the heap outlives it (it is never reset here).
    unsafe {
        let words = heap.alloc_words(2).expect("test heap has room for a float");
        write_float(words, value)
    }
}

/// A flat-map (`#{…}`, at most `MAP_SMALL_MAP_LIMIT` keys) term on the test heap.
fn flat_map(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    let words = map_words_capacity(pairs.len());
    // SAFETY: `alloc_words(map_words_capacity(len))` reserves the exact upper bound
    // `write_map` writes; every returned word is written by the call and the heap
    // outlives the term (it is never reset here).
    unsafe {
        let hp = heap
            .alloc_words(words)
            .expect("test heap has room for a map");
        write_map(hp, pairs, atoms).expect("a well-formed flat map")
    }
}

#[test]
fn guard_connectives_short_circuit_versus_strict_filter_a_run() {
    // erl:
    //   [{'$1',[{'andalso',{'>','$1',1},{'<','$1',10}}],['$1']}]
    //       over [1,3,5,10] -> [3,5]
    //   [{'$1',[{'orelse',{'<','$1',2},{'>','$1',9}}],['$1']}]
    //       over [1,3,5,10] -> [1,10]
    //   [{'$1',[{'not',{'<','$1',2}}],['$1']}] over [1,3] -> [3]
    //   [{'$1',[{'or',true,{'hd',5}}],['$1']}] over [1,3] -> []
    //       (`or` is STRICT: it evaluates the raising operand, so no element matches)
    //   [{'$1',[{'orelse',true,{'hd',5}}],['$1']}] over [1,3] -> [1,3]
    //       (`orelse` short-circuits at `true` and never evaluates the raise)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = at(&atoms, "$1");

    let input = list(
        &mut heap,
        &[make_small(1), make_small(3), make_small(5), make_small(10)],
    );
    let v = at(&atoms, "$1");

    let greater = tup(&mut heap, &[at(&atoms, ">"), v, make_small(1)]);
    let less_ten = tup(&mut heap, &[at(&atoms, "<"), v, make_small(10)]);
    let conjunction = tup(&mut heap, &[at(&atoms, "andalso"), greater, less_ten]);
    let got = run_one(input, head, &[conjunction], &[v], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[make_small(3), make_small(5)], &atoms);

    let less_two = tup(&mut heap, &[at(&atoms, "<"), v, make_small(2)]);
    let greater_nine = tup(&mut heap, &[at(&atoms, ">"), v, make_small(9)]);
    let disjunction = tup(&mut heap, &[at(&atoms, "orelse"), less_two, greater_nine]);
    let got = run_one(input, head, &[disjunction], &[v], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[make_small(1), make_small(10)], &atoms);

    let small = list(&mut heap, &[make_small(1), make_small(3)]);
    let less_two = tup(&mut heap, &[at(&atoms, "<"), v, make_small(2)]);
    let negation = tup(&mut heap, &[at(&atoms, "not"), less_two]);
    let got = run_one(small, head, &[negation], &[v], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[make_small(3)], &atoms);

    let raising = tup(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let strict_or = tup(
        &mut heap,
        &[at(&atoms, "or"), at(&atoms, "true"), raising],
    );
    let got = run_one(small, head, &[strict_or], &[v], &atoms, &mut heap).unwrap();
    assert!(
        got.is_empty(),
        "`or` is strict and the raising operand fails every clause"
    );

    let raising = tup(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let short = tup(
        &mut heap,
        &[at(&atoms, "orelse"), at(&atoms, "true"), raising],
    );
    let got = run_one(small, head, &[short], &[v], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[make_small(1), make_small(3)], &atoms);
}

#[test]
fn comparisons_are_exact_versus_arithmetic_over_a_run() {
    // erl on `[1, 1.0]` with guard `{Op,'$1',1}` and body `['$1']`:
    //   '=:=' -> [1]      '=/=' -> [1.0]     '==' -> [1,1.0]
    //   '=<'  -> [1,1.0]  '>='  -> [1,1.0]   '<'  -> []     '>'  -> []
    // (`=:=`/`=/=` are exact, so the float `1.0` differs from the integer `1`; the
    //  arithmetic family treats them as equal, per `beam_term::compare`.)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = at(&atoms, "$1");
    let v = at(&atoms, "$1");
    let one_point_zero = float(&mut heap, 1.0);
    let input = list(&mut heap, &[make_small(1), one_point_zero]);

    for (op, expected) in [
        ("=:=", vec![OneOrFloat::Int]),
        ("=/=", vec![OneOrFloat::Float]),
        ("==", vec![OneOrFloat::Int, OneOrFloat::Float]),
        ("=<", vec![OneOrFloat::Int, OneOrFloat::Float]),
        (">=", vec![OneOrFloat::Int, OneOrFloat::Float]),
        ("<", vec![]),
        (">", vec![]),
    ] {
        let comparison = tup(
            &mut heap,
            &[at(&atoms, op), v, make_small(1)],
        );
        let got = run_one(input, head, &[comparison], &[v], &atoms, &mut heap).unwrap();
        let want: Vec<Eterm> = expected
            .iter()
            .map(|which| match which {
                OneOrFloat::Int => make_small(1),
                OneOrFloat::Float => one_point_zero,
            })
            .collect();
        assert_terms_eq(&got, &want, &atoms);
    }
}

/// One of the two comparison operands in [`comparisons_are_exact_versus_arithmetic_over_a_run`].
#[derive(Clone, Copy)]
enum OneOrFloat {
    Int,
    Float,
}

#[test]
fn type_tests_and_guard_bifs_filter_a_run() {
    // erl:
    //   [{'$1',[{is_integer,'$1'}],['$1']}] over [1,a,2,1.0] -> [1,2]
    //   [{'$1',[{is_float,'$1'}],['$1']}]   over [1,a,2,1.0] -> [1.0]
    //   [{'$1',[{is_number,'$1'}],['$1']}]  over [1,a,2,1.0] -> [1,2,1.0]
    //   [{'$1',[{is_atom,'$1'}],['$1']}]    over [1,a,2]     -> [a]
    //   [{'$1',[{is_list,'$1'}],['$_']}]    over [[1,2],{1}] -> [[1,2]]
    //   [{'$1',[{is_tuple,'$1'}],['$_']}]   over [[1,2],{1}] -> [{1}]
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = at(&atoms, "$1");
    let v = at(&atoms, "$1");

    let one_point_zero = float(&mut heap, 1.0);
    let mixed = list(
        &mut heap,
        &[make_small(1), at(&atoms, "a"), make_small(2), one_point_zero],
    );
    for (test, want) in [
        (
            "is_integer",
            vec![make_small(1), make_small(2)],
        ),
        ("is_float", vec![one_point_zero]),
        (
            "is_number",
            vec![make_small(1), make_small(2), one_point_zero],
        ),
    ] {
        let guard = tup(&mut heap, &[at(&atoms, test), v]);
        let got = run_one(mixed, head, &[guard], &[v], &atoms, &mut heap).unwrap();
        assert_terms_eq(&got, &want, &atoms);
    }
    let atoms_only = list(&mut heap, &[make_small(1), at(&atoms, "a"), make_small(2)]);
    let guard = tup(&mut heap, &[at(&atoms, "is_atom"), v]);
    let got = run_one(atoms_only, head, &[guard], &[v], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[at(&atoms, "a")], &atoms);

    let inner = list(&mut heap, &[make_small(1), make_small(2)]);
    let one_tuple = tup(&mut heap, &[make_small(1)]);
    let shapes = list(&mut heap, &[inner, one_tuple]);
    let guard = tup(&mut heap, &[at(&atoms, "is_list"), v]);
    let got = run_one(shapes, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[inner], &atoms);
    let guard = tup(&mut heap, &[at(&atoms, "is_tuple"), v]);
    let got = run_one(shapes, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[one_tuple], &atoms);

    // Guard BIFs, each as an `=`/`=:=` comparison to a constant:
    //   {'=:=',{'length','$1'},3}       over [[1,2,3],[4,5]]   -> [[1,2,3]]
    //   {'=:=',{'hd','$1'},7}           over [[7,8],[9,8]]     -> [[7,8]]
    //   {'=:=',{'tl','$1'},[8]}         over [[7,8],[9,8]]     -> [[7,8],[9,8]]
    //   {'=:=',{abs,'$1'},7}            over [-7,7]            -> [-7,7]
    //   {'=:=',{'size','$1'},2}         over [{a,b},{a,b,c}]   -> [{a,b}]
    //   {'=:=',{'element',2,'$1'},b}    over [{a,b},{c,d}]     -> [{a,b}]
    let three = list(&mut heap, &[make_small(1), make_small(2), make_small(3)]);
    let two = list(&mut heap, &[make_small(4), make_small(5)]);
    let lists = list(&mut heap, &[three, two]);
    let length_call = tup(&mut heap, &[at(&atoms, "length"), v]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), length_call, make_small(3)]);
    let got = run_one(lists, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[three], &atoms);

    let pair_a = list(&mut heap, &[make_small(7), make_small(8)]);
    let pair_b = list(&mut heap, &[make_small(9), make_small(8)]);
    let pairs = list(&mut heap, &[pair_a, pair_b]);
    let hd_call = tup(&mut heap, &[at(&atoms, "hd"), v]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), hd_call, make_small(7)]);
    let got = run_one(pairs, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[pair_a], &atoms);

    let tl_call = tup(&mut heap, &[at(&atoms, "tl"), v]);
    let tail_only = list(&mut heap, &[make_small(8)]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), tl_call, tail_only]);
    let got = run_one(pairs, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    // Both pairs end in `[8]`, so the guard matches both (oracle:
    // `{'=:=',{'tl','$1'},[8]}` over `[[7,8],[9,8]]` -> `[[7,8],[9,8]]`).
    assert_terms_eq(&got, &[pair_a, pair_b], &atoms);

    let signed = list(&mut heap, &[make_small(-7), make_small(7)]);
    let abs_call = tup(&mut heap, &[at(&atoms, "abs"), v]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), abs_call, make_small(7)]);
    let got = run_one(signed, head, &[guard], &[v], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[make_small(-7), make_small(7)], &atoms);

    let two_tuple = tup(&mut heap, &[at(&atoms, "a"), at(&atoms, "b")]);
    let three_tuple = tup(
        &mut heap,
        &[at(&atoms, "a"), at(&atoms, "b"), at(&atoms, "c")],
    );
    let tuples = list(&mut heap, &[two_tuple, three_tuple]);
    let size_call = tup(&mut heap, &[at(&atoms, "size"), v]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), size_call, make_small(2)]);
    let got = run_one(tuples, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[two_tuple], &atoms);

    let c_d = tup(&mut heap, &[at(&atoms, "c"), at(&atoms, "d")]);
    let c_d_pair = list(&mut heap, &[two_tuple, c_d]);
    let element_call = tup(
        &mut heap,
        &[at(&atoms, "element"), make_small(2), v],
    );
    let guard = tup(
        &mut heap,
        &[at(&atoms, "=:="), element_call, at(&atoms, "b")],
    );
    let got = run_one(c_d_pair, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[two_tuple], &atoms);
}

#[test]
fn const_yields_its_argument_in_a_run() {
    // erl:
    //   [{'$1',[{'=:=',{const,5},5}],['$1']}] over [foo,1] -> [foo,1]
    //   [{'$1',[],[{const,ok}]}]               over [foo,1] -> [ok,ok]
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = at(&atoms, "$1");
    let v = at(&atoms, "$1");
    let input = list(&mut heap, &[at(&atoms, "foo"), make_small(1)]);

    let constant = tup(&mut heap, &[at(&atoms, "const"), make_small(5)]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), constant, make_small(5)]);
    let got = run_one(input, head, &[guard], &[v], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[at(&atoms, "foo"), make_small(1)], &atoms);

    let constant = tup(&mut heap, &[at(&atoms, "const"), at(&atoms, "ok")]);
    let got = run_one(input, head, &[], &[constant], &atoms, &mut heap).unwrap();
    assert_terms_eq(
        &got,
        &[at(&atoms, "ok"), at(&atoms, "ok")],
        &atoms,
    );
}

#[test]
fn a_failing_body_yields_exit_per_element_in_a_run() {
    // erl:
    //   [{'$1',[],[{'hd',5}]}]     over [1,2] -> ['EXIT','EXIT'] (the C `FAIL_TERM`)
    //   [{'$1',[],[{'hd',5},7]}]   over [1,2] -> [7,7] (the failure is discarded; the
    //                                                     last body expression supplies
    //                                                     the clause's value)
    let atoms = atoms_with_exit();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = at(&atoms, "$1");
    let input = list(&mut heap, &[make_small(1), make_small(2)]);

    let raising = tup(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let got = run_one(input, head, &[], &[raising], &atoms, &mut heap).unwrap();
    assert_terms_eq(
        &got,
        &[at(&atoms, "EXIT"), at(&atoms, "EXIT")],
        &atoms,
    );

    let raising = tup(&mut heap, &[at(&atoms, "hd"), make_small(5)]);
    let got = run_one(input, head, &[], &[raising, make_small(7)], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[make_small(7), make_small(7)], &atoms);
}

#[test]
fn dollar_zero_binds_and_a_malformed_dollar_atom_is_a_literal_in_a_run() {
    // erl:
    //   [{{'$0','_'},[],['$0']}]     over [{7,8}]     -> [7]     (`'$0'` IS variable 0)
    //   [{{'$x','_'},[],['$_']}]     over [{9,10}]    -> []      (an ordinary literal atom)
    //   [{{'$1','$x'},[],['$1']}]    over [{9,'$x'}]  -> [9]     (the literal matches)
    //   [{'$1',[],['$x']}]           over [1]         -> ['$x']  (a literal, not a variable)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let object = tup(&mut heap, &[make_small(7), make_small(8)]);
    let head = tup(&mut heap, &[at(&atoms, "$0"), at(&atoms, "_")]);
    let got = run_one(
        list(&mut heap, &[object]),
        head,
        &[],
        &[at(&atoms, "$0")],
        &atoms,
        &mut heap,
    )
    .unwrap();
    assert_terms_eq(&got, &[make_small(7)], &atoms);

    let object = tup(&mut heap, &[make_small(9), make_small(10)]);
    let head = tup(&mut heap, &[at(&atoms, "$x"), at(&atoms, "_")]);
    let got = run_one(
        list(&mut heap, &[object]),
        head,
        &[],
        &[at(&atoms, "$_")],
        &atoms,
        &mut heap,
    )
    .unwrap();
    assert!(got.is_empty(), "'$x' is a literal atom that matches no tuple key");

    let object = tup(&mut heap, &[make_small(9), at(&atoms, "$x")]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$x")]);
    let got = run_one(
        list(&mut heap, &[object]),
        head,
        &[],
        &[at(&atoms, "$1")],
        &atoms,
        &mut heap,
    )
    .unwrap();
    assert_terms_eq(&got, &[make_small(9)], &atoms);

    let got = run_one(
        list(&mut heap, &[make_small(1)]),
        at(&atoms, "$1"),
        &[],
        &[at(&atoms, "$x")],
        &atoms,
        &mut heap,
    )
    .unwrap();
    assert_terms_eq(
        &got,
        &[at(&atoms, "$x")],
        &atoms,
    );
}

#[test]
fn improper_and_nested_repeated_head_patterns_bind_in_a_run() {
    // erl:
    //   [{[10,20|'$1'],[],['$1']}]  over [[10,20,30]]   -> [[30]]
    //       (an improper LIST head is allowed; `'$1'` binds the tail)
    //   [{{'$1','$1'},[],['$1']}]   over [{7,7},{5,8}]  -> [7]
    //       (a repeated `'$N'` inside a nested tuple enforces equality)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let elements = list(&mut heap, &[make_small(10), make_small(20), make_small(30)]);
    let head = heap.cons(make_small(20), at(&atoms, "$1")).expect("heap");
    let head = heap.cons(make_small(10), head).expect("heap");
    let got = run_one(
        list(&mut heap, &[elements]),
        head,
        &[],
        &[at(&atoms, "$1")],
        &atoms,
        &mut heap,
    )
    .unwrap();
    let want = list(&mut heap, &[make_small(30)]);
    assert_terms_eq(&got, &[want], &atoms);

    let equal = tup(&mut heap, &[make_small(7), make_small(7)]);
    let unequal = tup(&mut heap, &[make_small(5), make_small(8)]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let got = run_one(
        list(&mut heap, &[equal, unequal]),
        head,
        &[],
        &[at(&atoms, "$1")],
        &atoms,
        &mut heap,
    )
    .unwrap();
    assert_terms_eq(&got, &[make_small(7)], &atoms);
}

#[test]
fn construct_bodies_build_fresh_terms_per_element_in_a_run() {
    // erl:
    //   [{{'$1','$2'},[],[{{'$2','$1'}}]}] over [{1,2}] -> [{2,1}]
    //   [{{'$1','$2'},[], [['$1','$2']]}]  over [{1,2}] -> [[1,2]]
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let object = tup(&mut heap, &[make_small(1), make_small(2)]);

    let swapped = tup(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$1")]);
    let body_element = tup(&mut heap, &[swapped]);
    let got = run_one(
        list(&mut heap, &[object]),
        head,
        &[],
        &[body_element],
        &atoms,
        &mut heap,
    )
    .unwrap();
    let want = tup(&mut heap, &[make_small(2), make_small(1)]);
    assert_terms_eq(&got, &[want], &atoms);

    let body_element = list(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let got = run_one(
        list(&mut heap, &[object]),
        head,
        &[],
        &[body_element],
        &atoms,
        &mut heap,
    )
    .unwrap();
    let want = list(&mut heap, &[make_small(1), make_small(2)]);
    assert_terms_eq(&got, &[want], &atoms);
}

#[test]
fn an_unbound_variable_makes_the_run_badarg() {
    // erl: `ets:match_spec_compile([{'$1',[{'=:=','$5',1}],['$1']}])` -> error:badarg;
    //      `ets:match_spec_compile([{'$1',[],['$5']}])` likewise. The whole call is
    //      `badarg` (the spec is rejected), so nothing runs.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = at(&atoms, "$1");
    let input = list(&mut heap, &[make_small(1)]);

    let guard = tup(&mut heap, &[at(&atoms, "=:="), at(&atoms, "$5"), make_small(1)]);
    assert!(
        matches!(
            run_one(input, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap),
            Err(BeamError::BadArg)
        ),
        "an unbound guard variable is badarg"
    );

    let head = at(&atoms, "$1");
    assert!(
        matches!(
            run_one(input, head, &[], &[at(&atoms, "$5")], &atoms, &mut heap),
            Err(BeamError::BadArg)
        ),
        "an unbound body variable is badarg"
    );
}

/// Two constructs that this crate's match-spec subset turns into a typed outcome but
/// the system `erl` (OTP 29 / erts-17.1) accepts. They are **recorded divergences** —
/// this crate is strictly *less* capable here — kept so the gap cannot silently change
/// sign; each is reported to the source owner rather than asserted as oracle behaviour.
#[test]
fn documented_gaps_diverge_from_the_oracle_in_a_run() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = at(&atoms, "$1");
    let input = list(&mut heap, &[make_small(1), make_small(2)]);

    // 1. A map as a body *expression* is **constructed** here, as the oracle does:
    //      `[{'$1',[],[#{a=>1}]}]` over `[1,2]` -> `[#{a=>1},#{a=>1}]`.
    let map = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(1))], &atoms);
    let got = run_one(input, head, &[], &[map], &atoms, &mut heap).unwrap();
    assert_terms_eq(&got, &[map, map], &atoms);

    // 2. `self/0` has no process identity at this layer, so it raises and the clause is
    //    skipped. oracle: `[{'$1',[{'=:=',{self},{self}}],['$1']}]` over `[1]` -> `[1]`.
    let self_call = tup(&mut heap, &[at(&atoms, "self")]);
    let guard = tup(
        &mut heap,
        &[at(&atoms, "=:="), self_call, self_call],
    );
    let one = list(&mut heap, &[make_small(1)]);
    let got = run_one(one, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap).unwrap();
    assert!(
        got.is_empty(),
        "DIVERGENCE: the oracle matches `{{self}} =:= {{self}}`; this crate's `self/0` raises"
    );
}

// ===========================================================================
// The guard-error classification over `ets:match_spec_run/2`
//
// The run-level mirror of `tests/select.rs`'s P0 guard tests: a guard this runtime
// CANNOT evaluate is a typed error (the run is `Err`, never a silent `Ok([])`); a
// guard the ORACLE itself raises merely skips the clause (`Ok([])`).
// ===========================================================================

#[test]
fn unevaluable_guard_is_a_typed_error() {
    // erl: `ets:match_spec_run([{1,10},{2,20}], ets:match_spec_compile([
    //      {{'$1','$2'},[{'=:=',{'/',4,2},2.0}],['$1']}]))` -> `[1,2]`
    // (`4/2` is `2.0`, so the guard holds and the body returns each key). This crate's
    // default `ArithHook` refuses `/` (its Erlang result is always a float), so the
    // guard cannot be evaluated: the run is the typed `unsupported_ets` raise — NEVER
    // `Ok([])`, which would be a silent under-match (the P0 guard-error classification).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let rhs = float(&mut heap, 2.0);
    let div = call(&mut heap, at(&atoms, "/"), &[make_small(4), make_small(2)]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), div, rhs]);
    let input = pair_list(&mut heap, &[(1, 10), (2, 20)]);
    let err = run_one(input, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap)
        .expect_err("a guard this runtime cannot evaluate is never a silent non-match");
    let raise = err
        .into_raise()
        .expect("the arithmetic refusal is an explicit raise");
    assert!(
        raise
            .reason
            .as_atom()
            .expect("the reason is an atom")
            .starts_with("unsupported_ets: match-spec arithmetic"),
        "the refusal names the unsupported arithmetic"
    );
}

#[test]
fn raising_guard_skips_the_clause_in_a_run() {
    // erl: `ets:match_spec_run([{1,10},{2,20}], ets:match_spec_compile([
    //      {{'$1','$2'},[{'=:=',{element,9,'$1'},1}],['$1']}]))` -> `[]`, and likewise
    // for `{hd,'$1'}` and `{'div',1,0}`. Each guard RAISES in the oracle (`element/2`
    // out of range, `hd` of a non-list, division by zero), so the clause is unmet and
    // no element matches: `Ok([])`, NOT a typed error.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let input = pair_list(&mut heap, &[(1, 10), (2, 20)]);
    let body = [at(&atoms, "$1")];

    let element9 = call(
        &mut heap,
        at(&atoms, "element"),
        &[make_small(9), at(&atoms, "$1")],
    );
    let guard = tup(&mut heap, &[at(&atoms, "=:="), element9, make_small(1)]);
    assert!(
        run_one(input, head, &[guard], &body, &atoms, &mut heap)
            .unwrap()
            .is_empty(),
        "element/2 out of range skips the clause"
    );

    let hd1 = call(&mut heap, at(&atoms, "hd"), &[at(&atoms, "$1")]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), hd1, make_small(1)]);
    assert!(
        run_one(input, head, &[guard], &body, &atoms, &mut heap)
            .unwrap()
            .is_empty(),
        "hd/1 of a non-list skips the clause"
    );

    let div0 = call(&mut heap, at(&atoms, "div"), &[make_small(1), make_small(0)]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), div0, make_small(1)]);
    assert!(
        run_one(input, head, &[guard], &body, &atoms, &mut heap)
            .unwrap()
            .is_empty(),
        "div by zero skips the clause"
    );
}
