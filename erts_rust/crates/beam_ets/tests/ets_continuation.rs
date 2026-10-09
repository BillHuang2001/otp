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
/// `a`..`f` are the object values; `<` is the guard operator).
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in ["_", "$1", "a", "b", "c", "d", "e", "f", "<"] {
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

/// `{K}` — the singleton object the direction probes use (as in the oracle's
/// `{1}`..`{6}` fixture).
fn singleton(heap: &mut Heap, key: i64) -> Eterm {
    heap.tuple(&[make_small(key)]).expect("test heap has room")
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

/// An `ordered_set` `{1}`..`{count}` — the singleton-object fixture the direction
/// and identity probes use (the oracle's `{1}`..`{6}`). `configure` may amend the
/// `ets:new/2` options (the rename probes build `named_table`s and name-recorded
/// tables this way).
fn singleton_table(
    heap: &mut Heap,
    atoms: &AtomTable,
    count: i64,
    configure: impl FnOnce(&mut TableOptions),
) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    configure(&mut opts);
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table");
    for key in 1..=count {
        let obj = singleton(heap, key);
        table.insert(obj, atoms).expect("insert into a fresh table");
    }
    table
}

/// The six `{1,a}`..`{6,f}` entries two full `limit = 3` chunks need.
const SIX: &[(i64, &str)] = &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e"), (6, "f")];

/// The signature every `/1` entry point shares with its siblings.
type ResumeStep = fn(
    &Table,
    &MatchContinuation,
    &AtomTable,
    &mut Heap,
) -> Result<(Vec<Eterm>, Option<MatchContinuation>), BeamError>;

/// The four `/1` entry points, in the order the embedded oracle escript's list
/// comprehensions name them. In the oracle all four are the **same** resumption —
/// this crate routes them through one identity/direction/body check — so the tests
/// below drive a single continuation through every one of them and require the
/// identical chunk.
const RESUMES: [(&str, ResumeStep); 4] = [
    ("select", Table::select_continue),
    ("select_reverse", Table::select_reverse_continue),
    ("match", Table::match_continue),
    ("match_object", Table::match_object_continue),
];/// `[{ '$1', [], ['$1'] }]` — a bare-variable head that binds the whole object and
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

/// The "deleted table" case: a continuation never transfers to a table created
/// *after* it, even one built identically — every [`Table`] gets its own monotonic
/// [`TableId`](beam_ets::TableId), so the oracle's `ets:delete/1`-then-recreate
/// scenario (`ets:select(Cont)` ⇒ `error:badarg`; probed) is modelled by the
/// identity check.
#[test]
fn a_continuation_does_not_transfer_to_a_recreated_table() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut out = Heap::new(4096).expect("out heap");
    let spec = whole_object_spec(&mut heap, &atoms);

    let original = ordered_table(&mut heap, &atoms, SIX);
    let (_chunk, cont) = original.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    drop(original); // `ets:delete/1`

    let recreated = ordered_table(&mut heap, &atoms, SIX);
    assert!(matches!(
        recreated.select_continue(&cont, &atoms, &mut out),
        Err(BeamError::BadArg)
    ));
    // ...and the id is never handed out again, so a later table rejects it too.
    let later = ordered_table(&mut heap, &atoms, SIX);
    assert!(matches!(
        later.select_continue(&cont, &atoms, &mut out),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// The direction and the body travel with the continuation, not with the `/1` name
// ---------------------------------------------------------------------------

/// **The continuation carries its own direction: the `/1` entry point's name does
/// not override it.** All four names yield the identical chunk for a continuation.
///
/// Oracle probe (OTP 29.1.1 / erts-17.1), `ordered_set {1}`..`{6}`,
/// `S = [{'_',[],['$_']}]`:
/// ```text
/// {FC, CFC} = ets:select(T, S, 3),          FC = [{1},{2},{3}]
/// ets:select_reverse(CFC)      -> {[{4},{5},{6}],cont}   ets:match(CFC)    -> same
/// {RC, CRC} = ets:select_reverse(T, S, 3),  RC = [{6},{5},{4}]
/// ets:select(CRC)              -> {[{3},{2},{1}],cont}   ets:match_object(CRC) -> same
/// ```
#[test]
fn the_resume_entry_point_does_not_override_the_carried_direction() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut out = Heap::new(4096).expect("out heap");
    let table = singleton_table(&mut heap, &atoms, 6, |_| {});
    let spec = whole_object_spec(&mut heap, &atoms);

    let forward_first = [
        singleton(&mut heap, 1),
        singleton(&mut heap, 2),
        singleton(&mut heap, 3),
    ];
    let forward_second = [
        singleton(&mut heap, 4),
        singleton(&mut heap, 5),
        singleton(&mut heap, 6),
    ];
    let reverse_first = [
        singleton(&mut heap, 6),
        singleton(&mut heap, 5),
        singleton(&mut heap, 4),
    ];
    let reverse_second = [
        singleton(&mut heap, 3),
        singleton(&mut heap, 2),
        singleton(&mut heap, 1),
    ];

    let (chunk, cont) = table.select_limit(&spec, 3, &atoms, &mut out).unwrap();
    assert_results(&chunk, &forward_first, &atoms);
    let cont = cont.expect("the limit landed before the end");
    for (via, resume) in RESUMES {
        let (next, more) = resume(&table, &cont, &atoms, &mut out)
            .unwrap_or_else(|error| panic!("{via}: {error:?}"));
        assert_results(&next, &forward_second, &atoms);
        // A full chunk still continues, and the *chained* resume keeps the
        // direction too.
        let more = more.unwrap_or_else(|| panic!("{via}: a full chunk still continues"));
        let (left, end) = resume(&table, &more, &atoms, &mut out)
            .unwrap_or_else(|error| panic!("{via} (chained): {error:?}"));
        assert!(left.is_empty() && end.is_none(), "{via}: exhausted");
    }

    let (chunk, cont) = table
        .select_reverse_limit(&spec, 3, &atoms, &mut out)
        .unwrap();
    assert_results(&chunk, &reverse_first, &atoms);
    let cont = cont.expect("the limit landed before the end");
    for (via, resume) in RESUMES {
        let (next, more) = resume(&table, &cont, &atoms, &mut out)
            .unwrap_or_else(|error| panic!("{via}: {error:?}"));
        assert_results(&next, &reverse_second, &atoms);
        let more = more.unwrap_or_else(|| panic!("{via}: a full reverse chunk continues"));
        let (left, end) = resume(&table, &more, &atoms, &mut out)
            .unwrap_or_else(|error| panic!("{via} (chained): {error:?}"));
        assert!(left.is_empty() && end.is_none(), "{via}: exhausted");
    }
}

/// The continuation carries its **body** (the compiled spec) as well, so the `/1`
/// name never changes what a chunk contains:
/// ```text
/// {_, MC} = ets:match(T, {'$1','_'}, 3),  MC = [[1],[2],[3]]
/// ets:select(MC), ets:match_object(MC)     -> {[[4],[5],[6]],cont}  (the BINDINGS)
/// {_, SC} = ets:select(T, S, 3),
/// ets:match(SC)                            -> {[{4,a},{5,a},{6,a}],cont}  (the body)
/// ```
#[test]
fn a_continuation_carries_its_body_across_the_match_and_select_entry_points() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut out = Heap::new(4096).expect("out heap");
    let table = ordered_table(&mut heap, &atoms, SIX);
    let spec = whole_object_spec(&mut heap, &atoms);
    let pattern = heap.tuple(&[at(&atoms, "$1"), at(&atoms, "_")]).unwrap();

    let bindings = |heap: &mut Heap, key: i64| heap.cons(make_small(key), NIL).unwrap();
    let want_bindings = [
        bindings(&mut heap, 4),
        bindings(&mut heap, 5),
        bindings(&mut heap, 6),
    ];
    let want_objects = [
        object(&mut heap, 4, at(&atoms, "d")),
        object(&mut heap, 5, at(&atoms, "e")),
        object(&mut heap, 6, at(&atoms, "f")),
    ];

    let (chunk, cont) = table.match_limit(pattern, 3, &atoms, &mut out).unwrap();
    assert_eq!(chunk.len(), 3, "the bound variables of the first three objects");
    let cont = cont.expect("a full chunk");
    for (via, resume) in RESUMES {
        let (next, _) = resume(&table, &cont, &atoms, &mut out)
            .unwrap_or_else(|error| panic!("{via}: {error:?}"));
        assert_results(&next, &want_bindings, &atoms);
    }

    let (chunk, cont) = table.select_limit(&spec, 3, &atoms, &mut out).unwrap();
    assert_eq!(chunk.len(), 3, "the spec's body of the first three objects");
    let cont = cont.expect("a full chunk");
    for (via, resume) in RESUMES {
        let (next, _) = resume(&table, &cont, &atoms, &mut out)
            .unwrap_or_else(|error| panic!("{via}: {error:?}"));
        assert_results(&next, &want_objects, &atoms);
    }
}

/// A continuation is an **immutable value**: the oracle hands back the same chunk
/// however often it is resumed, and a finished one keeps yielding the bare
/// `'$end_of_table'` (`{_, RE} = ets:select(T, S, 3)`: `ets:select(RE)` twice gives
/// the same `{chunk,'$end_of_table'}`; `{_, FI} = ets:select(T, S, 5)`:
/// `ets:select(FI)` gives `'$end_of_table'` every time).
#[test]
fn a_continuation_is_an_immutable_value() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut out = Heap::new(4096).expect("out heap");
    let table = ordered_table(&mut heap, &atoms, SIX);
    let spec = whole_object_spec(&mut heap, &atoms);

    // Resuming the same continuation twice yields the same chunk.
    let (_, cont) = table.select_limit(&spec, 3, &atoms, &mut out).unwrap();
    let cont = cont.expect("a full chunk");
    let (first, _) = table.select_continue(&cont, &atoms, &mut out).unwrap();
    let (second, _) = table.select_continue(&cont, &atoms, &mut out).unwrap();
    assert_results(&second, &first, &atoms);

    // A finished continuation keeps reporting the end.
    let (_, finished) = table.select_limit(&spec, 6, &atoms, &mut out).unwrap();
    let finished = finished.expect("landing exactly on the last object still continues");
    for _ in 0..3 {
        let (left, end) = table.select_continue(&finished, &atoms, &mut out).unwrap();
        assert!(left.is_empty() && end.is_none(), "the traversal is over");
    }
}

/// Reuse after the table changed: resuming over a table **emptied** since the
/// continuation was issued yields `([], None)` (the oracle's bare
/// `'$end_of_table'`), and objects **appended** past the issued position do not
/// disturb the resume (both probed on the system `erl`).
///
/// The oracle keeps a key-based cursor, so its resume also survives a deletion
/// *at or behind* the cursor; this crate's position is an index into the iteration
/// order, which does not. `src/select/continuation.rs` documents that as a known
/// approximation, so it is deliberately not pinned here.
#[test]
fn a_continuation_resumes_over_a_table_emptied_or_extended_since_it_was_issued() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut out = Heap::new(4096).expect("out heap");
    let spec = whole_object_spec(&mut heap, &atoms);

    // Emptied: the oracle's `ets:select(K1)` is the bare `'$end_of_table'`.
    let mut emptied = ordered_table(&mut heap, &atoms, SIX);
    let (_, cont) = emptied.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    emptied.delete_all_objects();
    let (results, end) = emptied.select_continue(&cont, &atoms, &mut out).unwrap();
    assert!(results.is_empty() && end.is_none());

    // Extended: the oracle continues at `[{3,a},{4,d}]`.
    let mut extended = ordered_table(&mut heap, &atoms, SIX);
    let (_, cont) = extended.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    extended
        .insert(object(&mut heap, 7, at(&atoms, "a")), &atoms)
        .expect("insert into a live table");
    let (results, more) = extended.select_continue(&cont, &atoms, &mut out).unwrap();
    assert_results(
        &results,
        &[
            object(&mut heap, 3, at(&atoms, "c")),
            object(&mut heap, 4, at(&atoms, "d")),
        ],
        &atoms,
    );
    assert!(more.is_some(), "the extended table still has objects left");
}

/// A **full final chunk** — the limit landing exactly on the table's last object —
/// still yields a continuation in every family, whose own resume is the empty end;
/// `limit > size` completes in one chunk instead, and a *partial* final chunk in the
/// reverse family ends. Probes on `{1}`..`{6}` (`{K,letter}` locally):
/// `ets:select(T,S,6)` / `ets:select_reverse(T,S,6)` / `ets:match(T,{'$1','_'},6)`
/// each return 6 results **plus a continuation**; `ets:select_reverse(T,S,5)` resumes
/// to the single remaining object; `ets:select(T,S,7)` returns
/// `{[6 objects],'$end_of_table'}`.
#[test]
fn a_full_final_chunk_still_yields_a_continuation_in_every_family() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut out = Heap::new(4096).expect("out heap");
    let table = ordered_table(&mut heap, &atoms, SIX);
    let spec = whole_object_spec(&mut heap, &atoms);
    let pattern = heap.tuple(&[at(&atoms, "$1"), at(&atoms, "_")]).unwrap();

    // limit == size: every family continues, and its resume is the empty end.
    let (r, c) = table.select_limit(&spec, 6, &atoms, &mut out).unwrap();
    assert_eq!(r.len(), 6);
    let (left, end) = table
        .select_continue(&c.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    assert!(left.is_empty() && end.is_none());

    let (r, c) = table
        .select_reverse_limit(&spec, 6, &atoms, &mut out)
        .unwrap();
    assert_eq!(r.len(), 6);
    let (left, end) = table
        .select_reverse_continue(&c.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    assert!(left.is_empty() && end.is_none());

    let (r, c) = table.match_limit(pattern, 6, &atoms, &mut out).unwrap();
    assert_eq!(r.len(), 6);
    let (left, end) = table
        .match_continue(&c.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    assert!(left.is_empty() && end.is_none());

    let (r, c) = table
        .match_object_limit(pattern, 6, &atoms, &mut out)
        .unwrap();
    assert_eq!(r.len(), 6);
    let (left, end) = table
        .match_object_continue(&c.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    assert!(left.is_empty() && end.is_none());

    // The reverse family's *partial* final chunk ends instead.
    let (r, c) = table
        .select_reverse_limit(&spec, 5, &atoms, &mut out)
        .unwrap();
    assert_eq!(r.len(), 5);
    let (left, end) = table
        .select_reverse_continue(&c.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    assert_results(&left, &[object(&mut heap, 1, at(&atoms, "a"))], &atoms);
    assert!(end.is_none(), "a partial final chunk ends the traversal");

    // limit > size completes with no continuation, in every family.
    assert!(table
        .select_limit(&spec, 7, &atoms, &mut out)
        .unwrap()
        .1
        .is_none());
    assert!(table
        .select_reverse_limit(&spec, 7, &atoms, &mut out)
        .unwrap()
        .1
        .is_none());
    assert!(table
        .match_limit(pattern, 7, &atoms, &mut out)
        .unwrap()
        .1
        .is_none());
    assert!(table
        .match_object_limit(pattern, 7, &atoms, &mut out)
        .unwrap()
        .1
        .is_none());
}

/// `limit == 0` is `error:badarg` in **all four** `/3` entry points (probed: the
/// oracle raises, it does not return an empty chunk).
#[test]
fn limit_zero_is_badarg_in_every_family() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut out = Heap::new(4096).expect("out heap");
    let table = ordered_table(&mut heap, &atoms, SIX);
    let spec = whole_object_spec(&mut heap, &atoms);
    let pattern = heap.tuple(&[at(&atoms, "$1"), at(&atoms, "_")]).unwrap();

    for (via, result) in [
        ("select", table.select_limit(&spec, 0, &atoms, &mut out)),
        (
            "select_reverse",
            table.select_reverse_limit(&spec, 0, &atoms, &mut out),
        ),
        ("match", table.match_limit(pattern, 0, &atoms, &mut out)),
        (
            "match_object",
            table.match_object_limit(pattern, 0, &atoms, &mut out),
        ),
    ] {
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "{via}: limit 0 must be badarg"
        );
    }
}

/// Continuation identity is the table's *user-facing identifier*, exactly as in the
/// oracle: a `named_table`'s continuation carries the **name atom**, so renaming the
/// table makes it unresolvable (`error:badarg`); a table that is not a `named_table`
/// — even one with a recorded name — keeps its continuation (it carries the runtime
/// id). Probe: `ets:select` on a continuation whose table was renamed gives
/// `{[{3},{4}],cont}` for an unnamed table and `error:badarg` for
/// `ets:new(nm,[ordered_set,named_table])` (whose continuation's first element is
/// the name atom, a `#Ref<…>` otherwise).
#[test]
fn a_named_tables_continuation_dies_with_its_name_but_an_unnamed_tables_survives() {
    let mut atoms = atoms();
    let named1 = atoms.intern("named1");
    let anon = atoms.intern("anon");
    let renamed = atoms.intern("renamed");
    let mut heap = Heap::new(8192).expect("test heap");
    let mut out = Heap::new(4096).expect("out heap");
    let spec = whole_object_spec(&mut heap, &atoms);

    // A `named_table`: the continuation dies with the name it was issued under.
    let mut named = singleton_table(&mut heap, &atoms, 6, |opts| {
        opts.set_named(true);
        opts.set_name(named1);
    });
    let (_, cont) = named.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    let (before, _) = named.select_continue(&cont, &atoms, &mut out).unwrap();
    assert_eq!(before.len(), 2, "valid while the name still resolves");
    named.rename(renamed, |_| false).expect("a free new name");
    assert!(matches!(
        named.select_continue(&cont, &atoms, &mut out),
        Err(BeamError::BadArg)
    ));

    // A recorded name alone is *not* the identity — the `named_table` flag decides.
    let mut recorded = singleton_table(&mut heap, &atoms, 6, |opts| {
        opts.set_name(anon);
    });
    let (_, cont) = recorded.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    recorded.rename(renamed, |_| false).expect("rename");
    let (after, more) = recorded.select_continue(&cont, &atoms, &mut out).unwrap();
    assert_eq!(after.len(), 2, "a name-recorded table is not a named_table");
    assert!(more.is_some());

    // ...and a plain unnamed table behaves the same way.
    let mut plain = singleton_table(&mut heap, &atoms, 6, |_| {});
    let (_, cont) = plain.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    plain.rename(renamed, |_| false).expect("rename");
    let (after, _) = plain.select_continue(&cont, &atoms, &mut out).unwrap();
    assert_eq!(after.len(), 2, "an unnamed table's continuation survives a rename");
}

// ---------------------------------------------------------------------------
// Reverse-traversal variants: `ets:select_reverse/3`+`select_reverse/1`
// ---------------------------------------------------------------------------
/// Reverse chunking on the `ordered_set {1,a}..{5,e}`, spec `[{'$1',[],['$1']}]`,
/// limit 2 — the oracle's `[{5,e},{4,d}]`+cont · `[{3,c},{2,b}]`+cont ·
/// `([{1,a}], None)` (the exact reverse of `ets:select/3`).
#[test]
fn select_reverse_limit_chunks_a_five_object_ordered_set() {
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
        object(&mut heap, 5, at(&atoms, "e")),
        object(&mut heap, 4, at(&atoms, "d")),
    ];
    let want2 = [
        object(&mut heap, 3, at(&atoms, "c")),
        object(&mut heap, 2, at(&atoms, "b")),
    ];
    let want3 = [object(&mut heap, 1, at(&atoms, "a"))];

    let (r1, c1) = table
        .select_reverse_limit(&spec, 2, &atoms, &mut out)
        .unwrap();
    assert_results(&r1, &want1, &atoms);
    let c1 = c1.expect("a full reverse chunk yields a continuation");

    let (r2, c2) = table.select_reverse_continue(&c1, &atoms, &mut out).unwrap();
    assert_results(&r2, &want2, &atoms);
    let c2 = c2.expect("another full reverse chunk yields a continuation");

    let (r3, c3) = table
        .select_reverse_continue(&c2, &atoms, &mut out)
        .unwrap();
    assert_results(&r3, &want3, &atoms);
    assert!(c3.is_none(), "the partial final reverse chunk carries no continuation");
}

/// The reverse-limit edges and the carried direction, quoted from the oracle:
/// the 4-object table's chunks end with the bare `'$end_of_table'`; `limit == size`
/// still continues (its resume is empty) while `limit > size` completes in one
/// chunk; `limit == 0` is `badarg`; an empty table and a guarded spec's partial
/// final chunk each yield no continuation; and the **direction travels with the
/// continuation** across both `/1` entry points (a reverse continuation stays
/// reverse via `ets:select/1` too).
#[test]
fn select_reverse_limit_edges_and_carried_direction() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = ordered_table(
        &mut heap,
        &atoms,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    let spec = whole_object_spec(&mut heap, &atoms);
    let mut out = Heap::new(4096).expect("out heap");

    // limit == size: a continuation is still returned; its resume yields nothing.
    let (r, c) = table.select_reverse_limit(&spec, 5, &atoms, &mut out).unwrap();
    assert_eq!(r.len(), 5);
    let c = c.expect("landing exactly on the last object still continues");
    let (left, end) = table.select_reverse_continue(&c, &atoms, &mut out).unwrap();
    assert!(left.is_empty() && end.is_none());

    // limit > size: one chunk, no continuation.
    let (r, c) = table.select_reverse_limit(&spec, 10, &atoms, &mut out).unwrap();
    assert_eq!(r.len(), 5);
    assert!(c.is_none());

    // limit == 0 is badarg; an empty table yields ([], None).
    assert!(matches!(
        table.select_reverse_limit(&spec, 0, &atoms, &mut out),
        Err(BeamError::BadArg)
    ));
    let empty = ordered_table(&mut heap, &atoms, &[]);
    let (r, c) = empty.select_reverse_limit(&spec, 2, &atoms, &mut out).unwrap();
    assert!(r.is_empty() && c.is_none());

    // The 4-object table's limit-2 chunks (the final resume is the bare end).
    let four = ordered_table(&mut heap, &atoms, &[(1, "a"), (2, "b"), (3, "c"), (4, "d")]);
    let (_, c) = four.select_reverse_limit(&spec, 2, &atoms, &mut out).unwrap();
    let (_, c) = four
        .select_reverse_continue(&c.expect("chunk 1"), &atoms, &mut out)
        .unwrap();
    let (r, c) = four
        .select_reverse_continue(&c.expect("chunk 2"), &atoms, &mut out)
        .unwrap();
    assert!(r.is_empty() && c.is_none(), "renders as the bare '$end_of_table'");

    // A guarded spec (keys 1..3): the partial final chunk carries no continuation.
    let guard = below_four_spec(&mut heap, &atoms);
    let (r, c) = table.select_reverse_limit(&guard, 2, &atoms, &mut out).unwrap();
    assert_results(&r, &[make_small(3), make_small(2)], &atoms);
    let (r, c) = table
        .select_reverse_continue(&c.expect("full chunk"), &atoms, &mut out)
        .unwrap();
    assert_results(&r, &[make_small(1)], &atoms);
    assert!(c.is_none());

    // The direction is carried: a reverse continuation stays reverse through both.
    let reverse_step = [
        object(&mut heap, 3, at(&atoms, "c")),
        object(&mut heap, 2, at(&atoms, "b")),
    ];
    let (_, rev) = table
        .select_reverse_limit(&spec, 2, &atoms, &mut out)
        .unwrap();
    let rev = rev.expect("a continuation");
    let (r, _) = table
        .select_reverse_continue(&rev, &atoms, &mut out)
        .unwrap();
    assert_results(&r, &reverse_step, &atoms);
    let (r, _) = table.select_continue(&rev, &atoms, &mut out).unwrap();
    assert_results(&r, &reverse_step, &atoms);
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
    {W1, Y1} = ets:select_reverse(T, S, 2),
    line("rev5_1", chunk(W1, Y1)),
    {W2, Y2} = ets:select_reverse(Y1),
    line("rev5_2", chunk(W2, Y2)),
    line("rev5_3", tail(ets:select_reverse(Y2))),
    line("rev_zero", try ets:select_reverse(T, S, 0) of _ -> ok catch _:_ -> badarg end),
    line("rev_empty", tail(ets:select_reverse(E, S, 2))),

    %% The direction AND the body travel with the continuation, not with the `/1`
    %% name; a continuation is an immutable value; its identity is the table's name.
    {_, DF} = ets:select(T, S, 3),
    [line(N, tail(F(DF))) || {N, F} <- [
        {"dir_fwd_via_select", fun ets:select/1},
        {"dir_fwd_via_select_reverse", fun ets:select_reverse/1},
        {"dir_fwd_via_match", fun ets:match/1},
        {"dir_fwd_via_match_object", fun ets:match_object/1}]],
    {_, DR} = ets:select_reverse(T, S, 3),
    [line(N, tail(F(DR))) || {N, F} <- [
        {"dir_rev_via_select", fun ets:select/1},
        {"dir_rev_via_select_reverse", fun ets:select_reverse/1},
        {"dir_rev_via_match", fun ets:match/1},
        {"dir_rev_via_match_object", fun ets:match_object/1}]],
    {_, BM} = ets:match(T, {'$1','_'}, 3),
    line("body_match_via_select", tail(ets:select(BM))),
    {_, BS} = ets:select(T, S, 3),
    line("body_select_via_match", tail(ets:match(BS))),
    {_, RE} = ets:select(T, S, 3),
    line("reuse_first", tail(ets:select(RE))),
    line("reuse_second", tail(ets:select(RE))),
    {_, FI} = ets:select(T, S, 5),
    line("finished_first", tail(ets:select(FI))),
    line("finished_second", tail(ets:select(FI))),
    {RV, RVC} = ets:select_reverse(T, S, 5),
    line("rev_full_final", chunk(RV, RVC)),
    line("rev_full_final_resume", tail(ets:select_reverse(RVC))),
    {RP, RPC} = ets:select_reverse(T, S, 3),
    line("rev_partial", chunk(RP, RPC)),
    line("rev_partial_resume", tail(ets:select_reverse(RPC))),
    UN = ets:new(un, [ordered_set]),
    [ets:insert(UN,{I}) || I <- lists:seq(1,6)],
    {_, UC} = ets:select(UN, S, 2),
    ets:rename(UN, un_renamed),
    line("unnamed_rename_resume", tail(ets:select(UC))),
    _NM = ets:new(nm, [ordered_set, named_table]),
    [ets:insert(nm,{I}) || I <- lists:seq(1,6)],
    {_, NC} = ets:select(nm, S, 2),
    ets:rename(nm, nm_renamed),
    line("named_rename_resume",
         try ets:select(NC) of _ -> "not-badarg" catch error:badarg -> "badarg" end),
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
    let mut atoms = atoms();
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

    let (r1, c1) = five
        .select_reverse_limit(&spec, 2, &atoms, &mut out)
        .unwrap();
    cases.insert("rev5_1".into(), render_chunk(&r1, c1.as_ref(), &atoms));
    let (r2, c2) = five
        .select_reverse_continue(&c1.unwrap(), &atoms, &mut out)
        .unwrap();
    cases.insert("rev5_2".into(), render_chunk(&r2, c2.as_ref(), &atoms));
    let (r3, c3) = five
        .select_reverse_continue(&c2.unwrap(), &atoms, &mut out)
        .unwrap();
    cases.insert("rev5_3".into(), render_chunk(&r3, c3.as_ref(), &atoms));

    let zero = five.select_reverse_limit(&spec, 0, &atoms, &mut out);
    cases.insert(
        "rev_zero".into(),
        match zero {
            Err(BeamError::BadArg) => "badarg".to_string(),
            other => panic!("reverse limit 0 should be badarg, got {other:?}"),
        },
    );

    let (r, c) = empty
        .select_reverse_limit(&spec, 2, &atoms, &mut out)
        .unwrap();
    cases.insert("rev_empty".into(), render_chunk(&r, c.as_ref(), &atoms));

    // ---- the direction and the body travel with the continuation -------------
    let (_, forward) = five.select_limit(&spec, 3, &atoms, &mut out).unwrap();
    let forward = forward.expect("a full chunk");
    for (via, resume) in RESUMES {
        let (chunk, cont) = resume(&five, &forward, &atoms, &mut out)
            .unwrap_or_else(|error| panic!("{via}: {error:?}"));
        cases.insert(
            format!("dir_fwd_via_{via}"),
            render_chunk(&chunk, cont.as_ref(), &atoms),
        );
    }
    let (_, reverse) = five
        .select_reverse_limit(&spec, 3, &atoms, &mut out)
        .unwrap();
    let reverse = reverse.expect("a full chunk");
    for (via, resume) in RESUMES {
        let (chunk, cont) = resume(&five, &reverse, &atoms, &mut out)
            .unwrap_or_else(|error| panic!("{via}: {error:?}"));
        cases.insert(
            format!("dir_rev_via_{via}"),
            render_chunk(&chunk, cont.as_ref(), &atoms),
        );
    }

    let (_, bindings) = five.match_limit(pattern, 3, &atoms, &mut out).unwrap();
    let (chunk, cont) = five
        .select_continue(&bindings.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    cases.insert(
        "body_match_via_select".into(),
        render_chunk(&chunk, cont.as_ref(), &atoms),
    );
    let (_, selected) = five.select_limit(&spec, 3, &atoms, &mut out).unwrap();
    let (chunk, cont) = five
        .match_continue(&selected.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    cases.insert(
        "body_select_via_match".into(),
        render_chunk(&chunk, cont.as_ref(), &atoms),
    );

    // ---- a continuation is an immutable value -------------------------------
    let (_, reuse) = five.select_limit(&spec, 3, &atoms, &mut out).unwrap();
    let reuse = reuse.expect("a full chunk");
    let (chunk, cont) = five.select_continue(&reuse, &atoms, &mut out).unwrap();
    cases.insert("reuse_first".into(), render_chunk(&chunk, cont.as_ref(), &atoms));
    let (chunk, cont) = five.select_continue(&reuse, &atoms, &mut out).unwrap();
    cases.insert("reuse_second".into(), render_chunk(&chunk, cont.as_ref(), &atoms));

    let (_, finished) = five.select_limit(&spec, 5, &atoms, &mut out).unwrap();
    let finished = finished.expect("limit == size still continues");
    let (chunk, cont) = five.select_continue(&finished, &atoms, &mut out).unwrap();
    cases.insert(
        "finished_first".into(),
        render_chunk(&chunk, cont.as_ref(), &atoms),
    );
    let (chunk, cont) = five.select_continue(&finished, &atoms, &mut out).unwrap();
    cases.insert(
        "finished_second".into(),
        render_chunk(&chunk, cont.as_ref(), &atoms),
    );

    // ---- full and partial final reverse chunks ------------------------------
    let (chunk, cont) = five
        .select_reverse_limit(&spec, 5, &atoms, &mut out)
        .unwrap();
    cases.insert(
        "rev_full_final".into(),
        render_chunk(&chunk, cont.as_ref(), &atoms),
    );
    let (chunk, cont) = five
        .select_reverse_continue(&cont.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    cases.insert(
        "rev_full_final_resume".into(),
        render_chunk(&chunk, cont.as_ref(), &atoms),
    );
    let (chunk, cont) = five
        .select_reverse_limit(&spec, 3, &atoms, &mut out)
        .unwrap();
    cases.insert("rev_partial".into(), render_chunk(&chunk, cont.as_ref(), &atoms));
    let (chunk, cont) = five
        .select_reverse_continue(&cont.expect("a full chunk"), &atoms, &mut out)
        .unwrap();
    cases.insert(
        "rev_partial_resume".into(),
        render_chunk(&chunk, cont.as_ref(), &atoms),
    );

    // ---- identity: renaming a named_table kills its continuation ------------
    let un_renamed = atoms.intern("un_renamed");
    let nm = atoms.intern("nm");
    let nm_renamed = atoms.intern("nm_renamed");
    let mut un = singleton_table(&mut heap, &atoms, 6, |_| {});
    let (_, cont) = un.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    un.rename(un_renamed, |_| false).expect("rename");
    let (chunk, cont) = un.select_continue(&cont, &atoms, &mut out).unwrap();
    cases.insert(
        "unnamed_rename_resume".into(),
        render_chunk(&chunk, cont.as_ref(), &atoms),
    );
    let mut named = singleton_table(&mut heap, &atoms, 6, |opts| {
        opts.set_named(true);
        opts.set_name(nm);
    });
    let (_, cont) = named.select_limit(&spec, 2, &atoms, &mut out).unwrap();
    let cont = cont.expect("a continuation");
    named.rename(nm_renamed, |_| false).expect("rename");
    cases.insert(
        "named_rename_resume".into(),
        match named.select_continue(&cont, &atoms, &mut out) {
            Err(BeamError::BadArg) => "badarg".to_string(),
            other => panic!("a renamed named_table must reject its continuation, got {other:?}"),
        },
    );

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
