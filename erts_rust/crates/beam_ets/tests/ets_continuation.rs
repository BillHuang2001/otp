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

//! The limit + continuation forms of the select family: `ets:select/3`+`select/1`,
//! `ets:match/3`+`match/1` and `ets:match_object/3`+`match_object/1`.
//!
//! Two layers of evidence:
//!
//! 1. **Fixed-expectation tests** — each expected value below was derived by
//!    running the identical operation against the system `erl` (OTP 29.1.1 /
//!    erts-17.1) with `escript`/`erl -noshell`; the probe and its literal output
//!    are quoted next to the assertion (the pattern used by `tests/ets_select.rs`
//!    and `tests/select.rs`). The chunking is exercised on an `ordered_set`, whose
//!    iteration order is deterministic (`tab2list` ascending); a hash-kind
//!    (`set`/`bag`/`duplicate_bag`) order is deliberately unspecified and is not
//!    pinned here.
//! 2. A **live-oracle cross-check** ([`rust_battery_agrees_with_the_live_oracle`])
//!    that runs the same battery of operations under the real `erl` (via the
//!    embedded `escript` below) and under `beam_ets`, comparing canonical
//!    renderings. It skips when no Erlang is installed; set
//!    `BEAM_ETS_REQUIRE_ORACLE=1` to make that skip a failure.
//!
//! # The rendering contract both sides share
//!
//! A `/3` call always returns `{results, cont}` when a full chunk was produced.
//! A `/1` resume returns either `{results, cont}` (another full chunk), the
//! 2-tuple `{results, '$end_of_table'}` (a partial final chunk) or the **bare**
//! atom `'$end_of_table'` (nothing left). The crate models the last two with
//! `Option::None`; the canonical rendering below makes that mapping explicit
//! (`Some` ⇒ `cont`, `None` with results ⇒ `end`, `None` empty ⇒ bare `end`).

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_ets::prelude::*;
use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The atoms the specs and values below reference (`$1` is the head variable;
/// `a`..`e` are the object values; `<` is the guard operator).
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in ["_", "$1", "a", "b", "c", "d", "e", "<"] {
        atoms.intern(name);
    }
    atoms
}

fn at(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).expect("interned by `atoms`"))
}

/// A `{K, V}` object with an atom value.
fn object(heap: &mut Heap, key: i64, value: Eterm) -> Eterm {
    heap.tuple(&[make_small(key), value]).expect("test heap has room")
}

/// An `ordered_set` of `{1,a}`..`{n,letter}` (deterministic iteration order).
fn ordered_table(heap: &mut Heap, atoms: &AtomTable, entries: &[(i64, &str)]) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table");
    for (key, value) in entries {
        let obj = object(heap, *key, at(atoms, value));
        table.insert(obj, atoms).expect("insert into a fresh table");
    }
    table
}

/// `[{ '$1', [], ['$1'] }]` — a bare-variable head that binds the whole object and
/// returns it. This is the spec the oracle chunking probes below use.
fn whole_object_spec(heap: &mut Heap, atoms: &AtomTable) -> MatchSpec {
    let head = at(atoms, "$1");
    let body = heap.cons(at(atoms, "$1"), NIL).unwrap();
    let clause = heap.tuple(&[head, NIL, body]).unwrap();
    let spec = heap.cons(clause, NIL).unwrap();
    compile(spec, atoms).expect("the quoted spec compiles")
}

/// `[{{'$1','_'}, [{'<','$1',4}], ['$1']}]` — bind the first element and keep it
/// only when it is below 4 (keys 1, 2, 3).
fn below_four_spec(heap: &mut Heap, atoms: &AtomTable) -> MatchSpec {
    let head = heap
        .tuple(&[at(atoms, "$1"), at(atoms, "_")])
        .unwrap();
    let guard = heap
        .tuple(&[at(atoms, "<"), at(atoms, "$1"), make_small(4)])
        .unwrap();
    let guards = heap.cons(guard, NIL).unwrap();
    let body = heap.cons(at(atoms, "$1"), NIL).unwrap();
    let clause = heap.tuple(&[head, guards, body]).unwrap();
    let spec = heap.cons(clause, NIL).unwrap();
    compile(spec, atoms).expect("the quoted spec compiles")
}

/// Assert two result vectors are element-wise `=:=`.
fn assert_results(got: &[Eterm], want: &[Eterm], atoms: &AtomTable) {
    assert_eq!(
        got.len(),
        want.len(),
        "result count: got {got:?}, want {want:?}"
    );
    for (index, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            eq(*g, *w, atoms),
            "result {index} differs: got {g:?}, want {w:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Fixed-expectation tests (each expectation quoted from the system `erl`)
// ---------------------------------------------------------------------------

/// 5-object `ordered_set {1,a}..{5,e}`, spec `[{'$1',[],['$1']}]`, limit 2.
///
/// `erl` probe (`escript`):
/// ```text
/// {R1, C1} = ets:select(T, S, 2),                       ->  [{1,a},{2,b}] ++ cont
/// {R2, C2} = ets:select(C1),                            ->  [{3,c},{4,d}] ++ cont
/// ets:select(C2)                                        ->  {[{5,e}],'$end_of_table'}
/// ```
#[test]
fn select_limit_chunks_a_five_object_ordered_set() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let spec = whole_object_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    let want1 = [
        object(&mut heap, 1, at(&atoms, "a")),
        object(&mut heap, 2, at(&atoms, "b")),
    ];
    let want2 = [
        object(&mut heap, 3, at(&atoms, "c")),
        object(&mut heap, 4, at(&atoms, "d")),
    ];
    let want3 = [object(&mut heap, 5, at(&atoms, "e"))];

    let (r1, c1) = table.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    assert_results(&r1, &want1, &atoms);
    let c1 = c1.expect("a full chunk yields a continuation");

    let (r2, c2) = table.select_continue(&c1, &atoms, &mut out).unwrap();
    assert_results(&r2, &want2, &atoms);
    let c2 = c2.expect("another full chunk yields a continuation");

    let (r3, c3) = table.select_continue(&c2, &atoms, &mut out).unwrap();
    assert_results(&r3, &want3, &atoms);
    assert!(c3.is_none(), "the partial final chunk carries no continuation");
}

/// 4-object table, limit 2: `2`+cont · `2`+cont · `([], None)`.
///
/// `erl` probe: `ets:select(T4, S, 2)` -> `[{1,a},{2,b}]`+cont, resume ->
/// `[{3,c},{4,d}]`+cont, resume -> the **bare** `'$end_of_table'`.
#[test]
fn select_limit_ends_with_an_empty_chunk() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d")],
    );
    let spec = whole_object_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    let (r1, c1) = table.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    assert_eq!(r1.len(), 2);
    let c1 = c1.expect("first chunk yields a continuation");

    let (r2, c2) = table.select_continue(&c1, &atoms, &mut out).unwrap();
    assert_eq!(r2.len(), 2);
    let c2 = c2.expect("second chunk yields a continuation");

    // The oracle's `ets:select(C2)` here is the bare `'$end_of_table'`.
    let (r3, c3) = table.select_continue(&c2, &atoms, &mut out).unwrap();
    assert!(r3.is_empty(), "the final resume finds nothing left");
    assert!(c3.is_none());
}

/// `limit >= table size` and the traversal finishes first -> `(all, None)`.
///
/// `erl`: `ets:select(T, S, 10)` -> `{[{1,a},…,{5,e}], '$end_of_table'}`.
#[test]
fn select_limit_at_or_above_the_table_size_returns_everything_and_ends() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let spec = whole_object_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    let (results, cont) = table.select_limit(&spec, 10, &atoms, &mut out).unwrap();
    assert_results(
        &results,
        &[
            object(&mut heap, 1, at(&atoms, "a")),
            object(&mut heap, 2, at(&atoms, "b")),
            object(&mut heap, 3, at(&atoms, "c")),
            object(&mut heap, 4, at(&atoms, "d")),
            object(&mut heap, 5, at(&atoms, "e")),
        ],
        &atoms,
    );
    assert!(cont.is_none(), "a completing traversal yields no continuation");
}

/// The limit lands **exactly** on the last object: the oracle still returns a
/// continuation.
///
/// `erl`: `ets:select(T, S, 5)` -> `{[{1,a},…,{5,e}], Cont}` (a real
/// continuation, not `'$end_of_table'`); resuming that continuation then yields
/// the bare `'$end_of_table'`.
#[test]
fn select_limit_landing_on_the_last_object_still_returns_a_continuation() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let spec = whole_object_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    let (results, cont) = table.select_limit(&spec, 5, &atoms, &mut out).unwrap();
    assert_eq!(results.len(), 5);
    let cont = cont.expect("landing exactly on the last object still continues");

    let (leftover, end) = table.select_continue(&cont, &atoms, &mut out).unwrap();
    assert!(leftover.is_empty());
    assert!(end.is_none());
}

/// `limit == 0` is `badarg`; an empty table yields `([], None)`.
///
/// `erl`: `ets:select(T, S, 0)` -> `error:badarg`; on an empty table
/// `ets:select(E, S, 2)` -> the bare `'$end_of_table'`.
#[test]
fn select_limit_rejects_zero_and_handles_an_empty_table() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let spec = whole_object_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    assert!(matches!(
        table.select_limit(&spec, 0, &atoms, &mut out),
        Err(BeamError::BadArg)
    ));

    let empty = ordered_table(&mut heap, &atoms, &[]);
    let (results, cont) = empty.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    assert!(results.is_empty());
    assert!(cont.is_none());
}

/// A partial final chunk (fewer than `limit` matches, scan reaches the end) yields
/// no continuation even though the traversal consumed only some objects.
///
/// `erl` with the guarded spec `[{{'$1','_'}, [{'<','$1',4}], ['$1']}]`
/// (matches keys 1..3): `ets:select(T, S, 2)` -> `[1,2]`+cont; resuming ->
/// `{[3],'$end_of_table'}`.
#[test]
fn select_limit_partial_final_chunk_has_no_continuation() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let spec = below_four_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    let (r1, c1) = table.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    assert_results(&r1, &[make_small(1), make_small(2)], &atoms);
    let c1 = c1.expect("a full chunk yields a continuation");

    let (r2, c2) = table.select_continue(&c1, &atoms, &mut out).unwrap();
    assert_results(&r2, &[make_small(3)], &atoms);
    assert!(c2.is_none(), "the scan reached the end with a partial chunk");
}

/// `Option::None` renders as the oracle does: the bare atom when empty, the
/// `{results, '$end_of_table'}` tuple otherwise. This pins the rendering handoff
/// the BIF bridge must implement.
#[test]
fn none_renders_as_the_end_of_table_contract() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(&mut heap, &atoms, &[(1, "a")]);
    let spec = whole_object_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    // A completing traversal with a non-empty result list -> `{results, end}`.
    let (results, cont) = table.select_limit(&spec, 5, &atoms, &mut out).unwrap();
    assert_eq!(results.len(), 1);
    assert!(cont.is_none());
    assert!(!results.is_empty(), "renders as {{results, '$end_of_table'}}");

    // An empty table -> `([], None)`, the bare `'$end_of_table'`.
    let empty = ordered_table(&mut heap, &atoms, &[]);
    let (results, cont) = empty.select_limit(&spec, 5, &atoms, &mut out).unwrap();
    assert!(results.is_empty(), "renders as the bare '$end_of_table'");
    assert!(cont.is_none());
}

/// `match_object/3`+`match_object/1` traverse the whole objects, chunked.
///
/// `erl`: `ets:match_object(T, {'$1','_'}, 2)` -> `[{1,a},{2,b}]`+cont, resume ->
/// `[{3,c},{4,d}]`+cont, resume -> `{[{5,e}],'$end_of_table'}`.
#[test]
fn match_object_limit_and_continue_traverse_the_whole_objects() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let pattern = heap
        .tuple(&[at(&atoms, "$1"), at(&atoms, "_")])
        .unwrap();
    let mut out = Heap::new(4096).expect("out heap");

    let (r1, c1) = table
        .match_object_limit(pattern, 2, &atoms, &mut out)
        .unwrap();
    assert_results(
        &r1,
        &[
            object(&mut heap, 1, at(&atoms, "a")),
            object(&mut heap, 2, at(&atoms, "b")),
        ],
        &atoms,
    );
    let c1 = c1.expect("first chunk yields a continuation");

    let (r2, c2) = table
        .match_object_continue(&c1, &atoms, &mut out)
        .unwrap();
    assert_results(
        &r2,
        &[
            object(&mut heap, 3, at(&atoms, "c")),
            object(&mut heap, 4, at(&atoms, "d")),
        ],
        &atoms,
    );
    let c2 = c2.expect("second chunk yields a continuation");

    let (r3, c3) = table
        .match_object_continue(&c2, &atoms, &mut out)
        .unwrap();
    assert_results(&r3, &[object(&mut heap, 5, at(&atoms, "e"))], &atoms);
    assert!(c3.is_none());

    // `limit == 0` is badarg here too.
    assert!(matches!(
        table.match_object_limit(pattern, 0, &atoms, &mut out),
        Err(BeamError::BadArg)
    ));
}

/// `match/3`+`match/1` traverse the bound variables, chunked.
///
/// `erl`: `ets:match(T, {'$1','_'}, 2)` -> `[[1],[2]]`+cont, resume ->
/// `[[3],[4]]`+cont, resume -> `{[[5]],'$end_of_table'}`.
#[test]
fn match_limit_and_continue_traverse_the_bound_variables() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let pattern = heap
        .tuple(&[at(&atoms, "$1"), at(&atoms, "_")])
        .unwrap();
    let mut out = Heap::new(4096).expect("out heap");

    let bindings = |heap: &mut Heap, key: i64| heap.cons(make_small(key), NIL).unwrap();

    let (r1, c1) = table.match_limit(pattern, 2, &atoms, &mut out).unwrap();
    assert_results(
        &r1,
        &[bindings(&mut heap, 1), bindings(&mut heap, 2)],
        &atoms,
    );
    let c1 = c1.expect("first chunk yields a continuation");

    let (r2, c2) = table.match_continue(&c1, &atoms, &mut out).unwrap();
    assert_results(
        &r2,
        &[bindings(&mut heap, 3), bindings(&mut heap, 4)],
        &atoms,
    );
    let c2 = c2.expect("second chunk yields a continuation");

    let (r3, c3) = table.match_continue(&c2, &atoms, &mut out).unwrap();
    assert_results(&r3, &[bindings(&mut heap, 5)], &atoms);
    assert!(c3.is_none());
}

/// A continuation offered to a **different** table is `badarg` rather than being
/// applied to unrelated objects (the continuation carries its table id).
#[test]
fn a_continuation_from_another_table_is_badarg() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let first = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d")],
    );
    let second = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d")],
    );
    let spec = whole_object_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    let (_r1, cont) = first.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    assert!(matches!(
        second.select_continue(&cont, &atoms, &mut out),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// Live-oracle cross-check
// ---------------------------------------------------------------------------

/// The embedded oracle program (an `escript`): it performs the battery below
/// against the real `ets` and prints one `name -> rendering` line per case. It
/// mirrors [`rust_battery`] operation for operation.
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    T = ets:new(t, [ordered_set]),
    [ets:insert(T, X) || X <- [{1,a},{2,b},{3,c},{4,d},{5,e}]],
    S = [{'$1', [], ['$1']}],
    {R1, C1} = ets:select(T, S, 2),
    line("select5_1", chunk(R1, C1)),
    {R2, C2} = ets:select(C1),
    line("select5_2", chunk(R2, C2)),
    line("select5_3", tail(ets:select(C2))),
    T4 = ets:new(t4, [ordered_set]),
    [ets:insert(T4, X) || X <- [{1,a},{2,b},{3,c},{4,d}]],
    {Q1, D1} = ets:select(T4, S, 2),
    line("select4_1", chunk(Q1, D1)),
    {Q2, D2} = ets:select(D1),
    line("select4_2", chunk(Q2, D2)),
    line("select4_3", tail(ets:select(D2))),
    line("select_ge", tail(ets:select(T, S, 10))),
    line("select_exact", tail(ets:select(T, S, 5))),
    E = ets:new(e, [ordered_set]),
    line("select_empty", tail(ets:select(E, S, 2))),
    line("select_zero", try ets:select(T, S, 0) of _ -> ok catch _:_ -> badarg end),
    G = [{{'$1','_'}, [{'<','$1',4}], ['$1']}],
    {G1, H1} = ets:select(T, G, 2),
    line("select_sub_1", chunk(G1, H1)),
    line("select_sub_2", tail(ets:select(H1))),
    {M1, N1} = ets:match(T, {'$1','_'}, 2),
    line("match_1", chunk(M1, N1)),
    {M2, N2} = ets:match(N1),
    line("match_2", chunk(M2, N2)),
    line("match_3", tail(ets:match(N2))),
    {O1, P1} = ets:match_object(T, {'$1','_'}, 2),
    line("match_object_1", chunk(O1, P1)),
    {O2, P2} = ets:match_object(P1),
    line("match_object_2", chunk(O2, P2)),
    line("match_object_3", tail(ets:match_object(P2))),
    ok.

chunk(Results, _Cont) -> "{" ++ rlist(Results) ++ ",cont}".
tail('$end_of_table') -> "end";
tail({Results, '$end_of_table'}) -> "{" ++ rlist(Results) ++ ",end}";
tail({Results, _Cont}) -> "{" ++ rlist(Results) ++ ",cont}".

line(Label, Rendered) -> io:format("~s -> ~s~n", [Label, Rendered]).

rlist(L) -> "[" ++ lists:join(",", [rterm(X) || X <- L]) ++ "]".
rtuple(T) -> "{" ++ lists:join(",", [rterm(E) || E <- tuple_to_list(T)]) ++ "}".
rterm(X) when is_integer(X) -> integer_to_list(X);
rterm(X) when is_atom(X) -> atom_to_list(X);
rterm(X) when is_list(X) -> rlist(X);
rterm(X) when is_tuple(X) -> rtuple(X).
"#;

/// Render the `beam_ets` battery's `{results, continuation}` pair exactly as the
/// embedded `escript` renders it (`Some` ⇒ `cont`; `None` non-empty ⇒ `end`;
/// `None` empty ⇒ the bare `end`).
fn render_chunk(
    results: &[Eterm],
    cont: Option<&MatchContinuation>,
    atoms: &AtomTable,
) -> String {
    let list = render_term_list(results, atoms);
    match cont {
        Some(_) => format!("{{{list},cont}}"),
        None if results.is_empty() => "end".to_string(),
        None => format!("{{{list},end}}"),
    }
}

fn render_term_list(results: &[Eterm], atoms: &AtomTable) -> String {
    let rendered: Vec<String> = results.iter().map(|t| render_term(*t, atoms)).collect();
    format!("[{}]", rendered.join(","))
}

/// The canonical term renderer (small integers, atoms, tuples, proper lists) the
/// `escript` also implements. Anything else is `<term>` (never produced by the
/// battery).
fn render_term(term: Eterm, atoms: &AtomTable) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if is_atom(term) {
        return match atoms.name(atom_val(term)) {
            Some(name) => name.to_string(),
            None => format!("atom#{:?}", atom_val(term)),
        };
    }
    if is_nil(term) {
        return "[]".to_string();
    }
    if is_list(term) {
        let mut parts = Vec::new();
        let mut cursor = term;
        while is_list(cursor) {
            // SAFETY: `is_list(cursor)` established a live cons cell, so its two
            // `[CAR, CDR]` words are readable; the term is table- or test-owned
            // and stays live for the call.
            unsafe {
                parts.push(render_term(car(cursor), atoms));
                cursor = cdr(cursor);
            }
        }
        // The battery only produces proper lists; an improper tail would render as
        // an explicit `.tail` marker.
        if !is_nil(cursor) {
            parts.push(format!(".{}", render_term(cursor, atoms)));
        }
        return format!("[{}]", parts.join(","));
    }
    // SAFETY: `is_tuple` reads only a boxed term's header and checks its shape.
    if unsafe { is_tuple(term) } {
        // SAFETY: `is_tuple` established a live tuple header.
        let arity = unsafe { tuple_size(term) };
        let parts: Vec<String> = (0..arity)
            .map(|index| {
                // SAFETY: `index < arity == tuple_size(term)`.
                render_term(unsafe { tuple_element(term, index) }, atoms)
            })
            .collect();
        return format!("{{{}}}", parts.join(","));
    }
    "<term>".to_string()
}

/// The `beam_ets` half of the battery — the same operations in the same order as
/// [`ORACLE_PROGRAM`].
fn rust_battery() -> BTreeMap<String, String> {
    let atoms = atoms();
    let mut heap = Heap::new(65536).expect("test heap");
    let mut out = Heap::new(65536).expect("out heap");
    let mut cases = BTreeMap::new();

    let five = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let spec = whole_object_spec(&mut heap, &atoms);

    let (r1, c1) = five.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    cases.insert("select5_1".into(), render_chunk(&r1, c1.as_ref(), &atoms));
    let (r2, c2) = five.select_continue(&c1.unwrap(), &atoms, &mut out).unwrap();
    cases.insert("select5_2".into(), render_chunk(&r2, c2.as_ref(), &atoms));
    let (r3, c3) = five.select_continue(&c2.unwrap(), &atoms, &mut out).unwrap();
    cases.insert("select5_3".into(), render_chunk(&r3, c3.as_ref(), &atoms));

    let four = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d")],
    );
    let (r1, c1) = four.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    cases.insert("select4_1".into(), render_chunk(&r1, c1.as_ref(), &atoms));
    let (r2, c2) = four.select_continue(&c1.unwrap(), &atoms, &mut out).unwrap();
    cases.insert("select4_2".into(), render_chunk(&r2, c2.as_ref(), &atoms));
    let (r3, c3) = four.select_continue(&c2.unwrap(), &atoms, &mut out).unwrap();
    cases.insert("select4_3".into(), render_chunk(&r3, c3.as_ref(), &atoms));

    let (r, c) = five.select_limit(&spec, 10, &atoms, &mut out).unwrap();
    cases.insert("select_ge".into(), render_chunk(&r, c.as_ref(), &atoms));
    let (r, c) = five.select_limit(&spec, 5, &atoms, &mut out).unwrap();
    cases.insert("select_exact".into(), render_chunk(&r, c.as_ref(), &atoms));

    let empty = ordered_table(&mut heap, &atoms, &[]);
    let (r, c) = empty.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    cases.insert("select_empty".into(), render_chunk(&r, c.as_ref(), &atoms));

    let zero = five.select_limit(&spec, 0, &atoms, &mut out);
    cases.insert(
        "select_zero".into(),
        match zero {
            Err(BeamError::BadArg) => "badarg".to_string(),
            other => panic!("limit 0 should be badarg, got {other:?}"),
        },
    );

    let guard_spec = below_four_spec(&mut heap, &atoms);
    let (r1, c1) = five.select_limit(&guard_spec, 2, &atoms, &mut out).unwrap();
    cases.insert("select_sub_1".into(), render_chunk(&r1, c1.as_ref(), &atoms));
    let (r2, c2) = five.select_continue(&c1.unwrap(), &atoms, &mut out).unwrap();
    cases.insert("select_sub_2".into(), render_chunk(&r2, c2.as_ref(), &atoms));

    let pattern = heap
        .tuple(&[at(&atoms, "$1"), at(&atoms, "_")])
        .unwrap();
    let (r1, c1) = five.match_limit(pattern, 2, &atoms, &mut out).unwrap();
    cases.insert("match_1".into(), render_chunk(&r1, c1.as_ref(), &atoms));
    let (r2, c2) = five.match_continue(&c1.unwrap(), &atoms, &mut out).unwrap();
    cases.insert("match_2".into(), render_chunk(&r2, c2.as_ref(), &atoms));
    let (r3, c3) = five.match_continue(&c2.unwrap(), &atoms, &mut out).unwrap();
    cases.insert("match_3".into(), render_chunk(&r3, c3.as_ref(), &atoms));

    let (r1, c1) = five
        .match_object_limit(pattern, 2, &atoms, &mut out)
        .unwrap();
    cases.insert("match_object_1".into(), render_chunk(&r1, c1.as_ref(), &atoms));
    let (r2, c2) = five
        .match_object_continue(&c1.unwrap(), &atoms, &mut out)
        .unwrap();
    cases.insert("match_object_2".into(), render_chunk(&r2, c2.as_ref(), &atoms));
    let (r3, c3) = five
        .match_object_continue(&c2.unwrap(), &atoms, &mut out)
        .unwrap();
    cases.insert("match_object_3".into(), render_chunk(&r3, c3.as_ref(), &atoms));

    cases
}

/// Locate the oracle's `escript` from `$ERTS_ORACLE`, then `$PATH`.
///
/// Mirroring `tests/differential.rs`, a `$ERTS_ORACLE` that points at something
/// non-executable is a **hard error** (a misconfiguration must not be mistaken for
/// "no oracle"), while a genuinely absent Erlang is a skip.
fn resolve_escript() -> Option<PathBuf> {
    let erl = match env::var_os("ERTS_ORACLE") {
        Some(value) => {
            let candidate = PathBuf::from(&value);
            assert!(
                is_executable(&candidate),
                "$ERTS_ORACLE is set to '{}', which is not an executable file",
                candidate.display()
            );
            candidate
        }
        None => find_in_path("erl")?,
    };
    let sibling = erl.with_file_name("escript");
    if is_executable(&sibling) {
        Some(sibling)
    } else {
        find_in_path("escript")
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Run the embedded `escript` and return its `name -> rendering` pairs.
fn run_oracle(escript: &Path) -> Vec<(String, String)> {
    let dir = env::temp_dir().join(format!("beam_ets_continuation_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the oracle scratch directory is creatable");
    let script = dir.join("beam_ets_continuation.escript");
    fs::write(&script, ORACLE_PROGRAM).expect("the oracle program is writable");

    let mut command = Command::new(escript);
    command.arg(&script).current_dir(&dir);
    let output = run_bounded(&mut command, Duration::from_secs(60)).unwrap_or_else(|error| {
        panic!("escript ({}) could not run the oracle: {error}", escript.display())
    });
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the oracle escript exited with {}:\n{stderr}",
        output.status
    );
    let _ = fs::remove_dir_all(&dir);

    parse_emitted(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `name -> rendering` lines into pairs, tolerating leading/trailing noise.
fn parse_emitted(stdout: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some((name, rendered)) = line.split_once(" -> ") {
            pairs.push((name.to_string(), rendered.trim().to_string()));
        }
    }
    pairs
}

/// Spawn `command` with both pipes captured and a wall-clock bound.
fn run_bounded(
    command: &mut Command,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    use std::process::Stdio;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|error| error.to_string()),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                return Err(format!("timed out after {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// The compatibility cross-check: the live oracle and `beam_ets` must render every
/// chunk identically.
#[test]
fn rust_battery_agrees_with_the_live_oracle() {
    let Some(escript) = resolve_escript() else {
        let note = "rust_battery_agrees_with_the_live_oracle: SKIPPED — no Erlang oracle \
                    found ($ERTS_ORACLE unset and no executable `escript` on $PATH).";
        if oracle_required() {
            panic!("{note} $BEAM_ETS_REQUIRE_ORACLE is set, so this is a failure.");
        }
        eprintln!("[continuation] {note}");
        return;
    };

    let oracle: BTreeMap<String, String> = run_oracle(&escript).into_iter().collect();
    let rust = rust_battery();

    assert_eq!(
        rust.keys().collect::<Vec<_>>(),
        oracle.keys().collect::<Vec<_>>(),
        "the two batteries must cover the same cases"
    );
    for (name, rust_value) in &rust {
        assert_eq!(
            rust_value, &oracle[name],
            "case {name}: beam_ets and the oracle disagree"
        );
    }
}

fn oracle_required() -> bool {
    match env::var("BEAM_ETS_REQUIRE_ORACLE") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}
