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
//! The **core chunking and immutability** coverage: how a `/3` limit splits the
//! traversal into chunks, how a `/1` resume continues (including the bare
//! `'$end_of_table'`), a continuation's immutability, its table identity, and the
//! reverse-traversal variants. The **direction/identity** half — that the `/1`
//! name never overrides the carried direction/body, and that a `named_table`'s
//! continuation dies with its name — lives in the sibling
//! `tests/ets_continuation_direction.rs`, together with the shared live-oracle
//! battery.
//!
//! # The rendering contract
//!
//! A `/3` call always returns `{results, cont}` when a full chunk was produced.
//! A `/1` resume returns either `{results, cont}` (another full chunk), the
//! 2-tuple `{results, '$end_of_table'}` (a partial final chunk) or the **bare**
//! atom `'$end_of_table'` (nothing left). The crate models the last two with
//! `Option::None`; the tests below render that mapping explicitly (`Some` ⇒ `cont`,
//! `None` with results ⇒ `end`, `None` empty ⇒ bare `end`).

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

/// The six `{1,a}`..`{6,f}` entries two full `limit = 3` chunks need.
const SIX: &[(i64, &str)] = &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e"), (6, "f")];

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
// Continuations as values: immutability, resumption after a table change, and
// the limit edge cases
// ---------------------------------------------------------------------------

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
