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

//! Selection: the `select_all` predicate and the compiled match-spec `select`
//! (wildcard/whole-object bodies, tuple-head binds, multi-element bodies,
//! guard-truth) plus `match_delete` and the reverse traversal `select_reverse`.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use beam_ets::{compile, MatchSpec, TableKind};
use beam_term::prelude::*;
use common::*;

#[test]
fn select_all_keeps_objects_accepted_by_the_predicate() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    for k in 0..5 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    // Keep objects whose first element (the key) is a small int > 2.
    let mut pred = |elements: &[Eterm]| is_small(elements[0]) && signed_value(elements[0]) > 2;
    let kept = t.select_all(&mut pred);

    assert_eq!(kept.len(), 2);
    for k in [3i64, 4] {
        let want = object(&mut heap, k, k * 10);
        assert!(
            kept.iter().any(|g| eq(*g, want, &atoms)),
            "key {k} should have been kept"
        );
    }
    for k in [0i64, 1, 2] {
        let not_want = object(&mut heap, k, k * 10);
        assert!(
            !kept.iter().any(|g| eq(*g, not_want, &atoms)),
            "key {k} should not have been kept"
        );
    }
}

#[test]
fn select_wildcard_head_and_whole_object_body() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    for k in 0..4 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    // Spec: [ {'_', [], ['$_']} ] — every object, returned whole. The body must be
    // a *list*: a bare atom body (`{'_', [], '$_'}`) is `badarg` in real ETS
    // (verified: `erl` raises `badarg`), and `beam_ets` correctly rejects it.
    let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "$_")]);
    let clause = heap
        .tuple(&[interned_atom(&mut atoms, "_"), NIL, body])
        .unwrap();
    let spec = proper_list(&mut heap, &[clause]);
    let compiled = compile(spec, &atoms).unwrap();

    let selected = t.select(&compiled, &atoms, &mut beam_alloc::Heap::new(4096).expect("heap")).unwrap();
    assert_eq!(selected.len(), 4);
    for k in 0..4 {
        let want = object(&mut heap, k, k * 10);
        assert!(
            selected.iter().any(|g| eq(*g, want, &atoms)),
            "key {k} should have been selected"
        );
    }
}

#[test]
fn select_tuple_head_with_bind_and_single_element_body() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    for k in 0..5 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    // Spec: [ {{'$1', '_'}, [], ['$1']} ] — the key of every object. (A body
    // element is an *expression*: the bare atom `'$1'`, not a one-element tuple.)
    let head = heap
        .tuple(&[
            interned_atom(&mut atoms, "$1"),
            interned_atom(&mut atoms, "_"),
        ])
        .unwrap();
    let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "$1")]);
    let clause = heap.tuple(&[head, NIL, body]).unwrap();
    let spec = proper_list(&mut heap, &[clause]);
    let compiled = compile(spec, &atoms).unwrap();

    let mut keys = t.select(&compiled, &atoms, &mut beam_alloc::Heap::new(4096).expect("heap")).unwrap();
    keys.sort_by(|a, b| cmp(*a, *b, &atoms));
    assert_eq!(keys, (0..5).map(make_small).collect::<Vec<_>>());
}

#[test]
fn select_multi_element_body_returns_only_the_last_value() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::OrderedSet);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 2, 20), &atoms).unwrap();

    // A body is a *sequence*: its value is the value of the LAST expression, so a
    // clause yields exactly one result per matched object. Verified against `erl`:
    // `ets:select(T, [{{'$1','$2'}, [], ['$2','$1']}])` on `{1,10}`/`{2,20}` returns
    // `[1,2]` (the values of `'$1'`), NOT `[10,1,20,2]`. (An unbound `'$N'` in a
    // body — e.g. head `'_'` with body `['$1']` — is `badarg`, also verified.)
    let body = proper_list(
        &mut heap,
        &[
            interned_atom(&mut atoms, "$2"),
            interned_atom(&mut atoms, "$1"),
        ],
    );
    let head = heap
        .tuple(&[
            interned_atom(&mut atoms, "$1"),
            interned_atom(&mut atoms, "$2"),
        ])
        .unwrap();
    let clause = heap.tuple(&[head, NIL, body]).unwrap();
    let spec = proper_list(&mut heap, &[clause]);
    let compiled = compile(spec, &atoms).unwrap();

    let results = t
        .select(&compiled, &atoms, &mut beam_alloc::Heap::new(4096).expect("heap"))
        .unwrap();
    assert_eq!(results, vec![make_small(1), make_small(2)]);
}

/// A guard's truth is computed internally, so it holds even when the caller's
/// `AtomTable` has never interned `true`/`false` (the bug: resolving the guard
/// result by name made every guard fail on an unseeded table).
#[test]
fn guard_truth_does_not_require_true_false_atoms() {
    let mut atoms = AtomTable::new();
    // Intern only the atoms the spec names — deliberately NOT `true`/`false`.
    for name in ["$1", "$2", "<", "is_integer"] {
        atoms.intern(name);
    }
    assert!(atoms.lookup("true").is_none());
    assert!(atoms.lookup("false").is_none());

    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::OrderedSet);
    for k in 0..4 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    // A comparison guard: keep objects whose value (`'$2'`) is below 25, so keys
    // 0, 1, 2 (values 0, 10, 20).
    let head = heap
        .tuple(&[
            interned_atom(&mut atoms, "$1"),
            interned_atom(&mut atoms, "$2"),
        ])
        .unwrap();
    let guard = heap
        .tuple(&[
            interned_atom(&mut atoms, "<"),
            interned_atom(&mut atoms, "$2"),
            make_small(25),
        ])
        .unwrap();
    let guards = proper_list(&mut heap, &[guard]);
    let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "$1")]);
    let clause = heap.tuple(&[head, guards, body]).unwrap();
    let spec = proper_list(&mut heap, &[clause]);
    let compiled = compile(spec, &atoms).unwrap();
    assert_eq!(
        t.select(&compiled, &atoms, &mut beam_alloc::Heap::new(4096).expect("heap"))
            .unwrap(),
        vec![make_small(0), make_small(1), make_small(2)]
    );

    // A type test works the same way. `is_integer` on every value keeps all four
    // objects (the atoms still lack `true`/`false`).
    let head = heap
        .tuple(&[
            interned_atom(&mut atoms, "$1"),
            interned_atom(&mut atoms, "$2"),
        ])
        .unwrap();
    let guard = heap
        .tuple(&[
            interned_atom(&mut atoms, "is_integer"),
            interned_atom(&mut atoms, "$2"),
        ])
        .unwrap();
    let guards = proper_list(&mut heap, &[guard]);
    let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "$1")]);
    let clause = heap.tuple(&[head, guards, body]).unwrap();
    let spec = proper_list(&mut heap, &[clause]);
    let compiled = compile(spec, &atoms).unwrap();
    let all = t
        .select(&compiled, &atoms, &mut beam_alloc::Heap::new(4096).expect("heap"))
        .unwrap();
    assert_eq!(all, (0..4).map(make_small).collect::<Vec<_>>());
}

/// `match_delete/2` deletes the matching objects and returns the number deleted.
/// (`ets:match_delete/2` itself returns the atom `true`; this crate's
/// `Table::match_delete` returns the count — see its documentation.)
#[test]
fn match_delete_removes_the_matching_objects_and_returns_the_count() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    // `{1, '_'}` matches exactly the object with key 1.
    let pattern = heap
        .tuple(&[make_small(1), interned_atom(&mut atoms, "_")])
        .unwrap();
    let mut out = beam_alloc::Heap::new(4096).expect("heap");
    assert_eq!(t.match_delete(pattern, &atoms, &mut out).unwrap(), 1);
    assert_eq!(t.size(), 2);
    assert!(!t.member(make_small(1), &atoms));

    // A pattern that matches nothing deletes nothing and returns 0.
    let absent = heap
        .tuple(&[make_small(99), interned_atom(&mut atoms, "_")])
        .unwrap();
    assert_eq!(t.match_delete(absent, &atoms, &mut out).unwrap(), 0);
    assert_eq!(t.size(), 2);

    // A bare variable pattern matches every object.
    assert_eq!(
        t.match_delete(interned_atom(&mut atoms, "$1"), &atoms, &mut out)
            .unwrap(),
        2
    );
    assert!(t.is_empty());
}

// ---------------------------------------------------------------------------
// `ets:select_reverse/2` — the reverse traversal
// ---------------------------------------------------------------------------

/// `ets:select_reverse/2` walks an `ordered_set` in **descending** numeric key
/// order.
///
/// `erl` probe: `ets:select_reverse(T, [{'$1',[],['$1']}])` on the `ordered_set`
/// `{1,1}..{5,5}` returns `[{5,5},{4,4},{3,3},{2,2},{1,1}]` (the exact reverse of
/// `ets:select/2`'s `[{1,1},…,{5,5}]`).
#[test]
fn select_reverse_descends_an_ordered_set() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1024);
    let mut t = new_table(TableKind::OrderedSet);
    for k in 1..=5 {
        t.insert(object(&mut heap, k, k), &atoms).unwrap();
    }
    let spec = whole_object_spec(&mut heap, &mut atoms);

    let got = t.select_reverse(&spec, &atoms, &mut out_heap()).unwrap();
    let want: Vec<Eterm> = (1..=5).rev().map(|k| object(&mut heap, k, k)).collect();
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(&want) {
        assert!(eq(*g, *w, &atoms), "expected {w:?} at this position, got {g:?}");
    }
}

/// The reverse traversal is a **complete permutation** of the forward one on every
/// table kind: on an `ordered_set` it is exactly the reverse sequence, and on the
/// hash kinds (`set`/`bag`/`duplicate_bag`, whose forward order is deliberately
/// unspecified) the sorted multiset is unchanged.
///
/// `erl` probe: on each of the four kinds over `{1,10}..{5,50}` the multiset of
/// `ets:select_reverse(T, [{{'$1','_'},[],['$1']}])` equals
/// `lists:sort(ets:select(T, …))` — `[1,2,3,4,5]`.
#[test]
fn select_reverse_is_a_complete_permutation_on_every_kind() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(1024);
        let mut t = new_table(kind);
        for k in 1..=5 {
            t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
        }
        let spec = key_spec(&mut heap, &mut atoms);

        let mut keys: Vec<i64> = t
            .select_reverse(&spec, &atoms, &mut out_heap())
            .unwrap()
            .iter()
            .map(|t| signed_value(*t))
            .collect();

        if kind.is_ordered() {
            // Exact: descending numeric key order, the reverse of `tab2list`.
            assert_eq!(keys, vec![5, 4, 3, 2, 1], "ordered_set must descend");
        } else {
            // A complete permutation of the same multiset (order unspecified).
            keys.sort();
            assert_eq!(keys, vec![1, 2, 3, 4, 5], "{kind:?} must be a permutation");
        }
    }
}

/// An empty table yields no results; a non-matching spec contributes nothing.
#[test]
fn select_reverse_handles_an_empty_and_a_non_matching_table() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1024);
    let empty = new_table(TableKind::OrderedSet);
    let spec = whole_object_spec(&mut heap, &mut atoms);
    assert!(empty.select_reverse(&spec, &atoms, &mut out_heap()).unwrap().is_empty());

    // A spec that matches nothing: head `{'$1', 99}` on the `ordered_set`
    // `{1,1}..{3,3}` binds the key but requires the value to be 99, so no object
    // matches (the body's `'$1'` stays bound, so the spec compiles).
    let mut t = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        t.insert(object(&mut heap, k, k), &atoms).unwrap();
    }
    let head = heap
        .tuple(&[interned_atom(&mut atoms, "$1"), make_small(99)])
        .unwrap();
    let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "$1")]);
    let clause = heap.tuple(&[head, NIL, body]).unwrap();
    let none_spec = compile(proper_list(&mut heap, &[clause]), &atoms).unwrap();
    assert!(t.select_reverse(&none_spec, &atoms, &mut out_heap()).unwrap().is_empty());
}

/// The live-oracle cross-check: the embedded `escript` runs `ets:select_reverse/2`
/// on all four kinds and the Rust side must render the identical results.
///
/// For an `ordered_set` the sequence is exact (descending); for the hash kinds the
/// order is unspecified, so both sides sort before rendering.
const REVERSE_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    S = [{{'$1','_'}, [], ['$1']}],
    Objs = [{K, K * 10} || K <- lists:seq(1, 5)],
    Rev = fun(Kind) ->
        T = ets:new(t, [Kind]),
        [ets:insert(T, O) || O <- Objs],
        ets:select_reverse(T, S)
    end,
    io:format("ordered_set -> ~w~n", [Rev(ordered_set)]),
    io:format("set -> ~w~n", [lists:sort(Rev(set))]),
    io:format("bag -> ~w~n", [lists:sort(Rev(bag))]),
    io:format("duplicate_bag -> ~w~n", [lists:sort(Rev(duplicate_bag))]),
    ok.
"#;

#[test]
fn select_reverse_agrees_with_the_live_oracle_across_kinds() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "select_reverse_agrees_with_the_live_oracle_across_kinds",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        REVERSE_ORACLE_PROGRAM,
        &[],
        "select_reverse",
        Duration::from_secs(60),
    );
    let expected: BTreeMap<String, String> =
        parse_lines(&String::from_utf8_lossy(&output.stdout));

    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1024);
    let spec = key_spec(&mut heap, &mut atoms);

    for (kind, name, sort) in [
        (TableKind::OrderedSet, "ordered_set", false),
        (TableKind::Set, "set", true),
        (TableKind::Bag, "bag", true),
        (TableKind::DuplicateBag, "duplicate_bag", true),
    ] {
        let mut t = new_table(kind);
        for k in 1..=5 {
            t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
        }
        let mut keys: Vec<i64> = t
            .select_reverse(&spec, &atoms, &mut out_heap())
            .unwrap()
            .iter()
            .map(|t| signed_value(*t))
            .collect();
        if sort {
            keys.sort();
        }
        let rendered = format!(
            "[{}]",
            keys.iter()
                .map(|k| k.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(rendered, expected[name], "kind {name} disagrees with the oracle");
    }
}

// ---------------------------------------------------------------------------
// `select_delete`/`match_delete`/`select_count` — which objects go, and the
// return count
// ---------------------------------------------------------------------------

/// The four kinds, in the delete battery's order.
const KINDS: [TableKind; 4] = [
    TableKind::Set,
    TableKind::OrderedSet,
    TableKind::Bag,
    TableKind::DuplicateBag,
];

/// `[{{'$1','_'}, [{'>','$1',1},{'<','$1',5}], [true]}]` — a literal-`true` body
/// whose guard keeps only the keys `2,3,4`.
fn guarded_true_spec(heap: &mut Heap, atoms: &mut AtomTable) -> MatchSpec {
    let head = heap
        .tuple(&[
            interned_atom(atoms, "$1"),
            interned_atom(atoms, "_"),
        ])
        .unwrap();
    let gt = heap
        .tuple(&[
            interned_atom(atoms, ">"),
            interned_atom(atoms, "$1"),
            make_small(1),
        ])
        .unwrap();
    let lt = heap
        .tuple(&[
            interned_atom(atoms, "<"),
            interned_atom(atoms, "$1"),
            make_small(5),
        ])
        .unwrap();
    let guards = proper_list(heap, &[gt, lt]);
    let body = proper_list(heap, &[interned_atom(atoms, "true")]);
    let clause = heap.tuple(&[head, guards, body]).unwrap();
    compile(proper_list(heap, &[clause]), atoms).expect("the guarded spec compiles")
}

/// `[{{'$1','$2'}, [], [true]}]` — a literal-`true` body over every 2-tuple object.
fn all_true_spec(heap: &mut Heap, atoms: &mut AtomTable) -> MatchSpec {
    let head = heap
        .tuple(&[
            interned_atom(atoms, "$1"),
            interned_atom(atoms, "$2"),
        ])
        .unwrap();
    let body = proper_list(heap, &[interned_atom(atoms, "true")]);
    let clause = heap.tuple(&[head, NIL, body]).unwrap();
    compile(proper_list(heap, &[clause]), atoms).expect("the all-true spec compiles")
}

/// The delete/return-value facts of one kind.
///
/// On `{1,10}..{5,50}`: `select_count` and `select_delete` over the guard
/// `1 < key < 5` agree (3 objects, keys 2/3/4 removed, leaving `[1,5]`); a
/// `match_delete({5, '_'})` removes the one object keyed by 5 and reports **1** —
/// the crate returns the deleted count where the BIF returns `true` (see
/// [`DELETE_ORACLE_RETURN`]); and a fresh table holding three same-key objects
/// (`{1,10}/{1,20}/{1,30}`) reports the per-kind object count, deletes them all and
/// empties — proving the count is objects, never keys.
fn delete_facts(kind: TableKind) -> BTreeMap<String, String> {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(2048);
    let name = kind.as_atom_name();
    let mut facts = BTreeMap::new();

    let mut table = new_table(kind);
    for object in objects_1_to_5(&mut heap) {
        table.insert(object, &atoms).unwrap();
    }

    let guarded = guarded_true_spec(&mut heap, &mut atoms);
    let count = table.select_count(&guarded, &atoms, &mut out_heap()).unwrap();
    let deleted = table.select_delete(&guarded, &atoms, &mut out_heap()).unwrap();
    facts.insert(format!("{name}.select_count"), count.to_string());
    facts.insert(format!("{name}.select_delete"), deleted.to_string());
    facts.insert(
        format!("{name}.after_select_delete"),
        render_ints(&sorted_keys(&table)),
    );

    let pattern = heap
        .tuple(&[make_small(5), interned_atom(&mut atoms, "_")])
        .unwrap();
    let matched = table.match_delete(pattern, &atoms, &mut out_heap()).unwrap();
    facts.insert(format!("{name}.match_delete_count"), matched.to_string());
    facts.insert(
        format!("{name}.after_match_delete"),
        render_ints(&sorted_keys(&table)),
    );

    // Size counts *objects*: three inserts sharing one key.
    let mut multi = new_table(kind);
    for value in [10i64, 20, 30] {
        multi.insert(object(&mut heap, 1, value), &atoms).unwrap();
    }
    let all = all_true_spec(&mut heap, &mut atoms);
    facts.insert(format!("{name}.objs.size"), multi.size().to_string());
    let removed = multi.select_delete(&all, &atoms, &mut out_heap()).unwrap();
    facts.insert(format!("{name}.objs.select_delete_all"), removed.to_string());
    facts.insert(
        format!("{name}.objs.size_after"),
        multi.size().to_string(),
    );
    facts
}

/// Every delete fact the crate produces — the figures printed by the system `erl`
/// (OTP 29.1.1 / erts-17.1) for the identical script.
fn all_delete_facts() -> BTreeMap<String, String> {
    let mut facts = BTreeMap::new();
    for kind in KINDS {
        facts.extend(delete_facts(kind));
    }
    facts
}

/// The delete facts the crate and the oracle share. (`match_delete/2`'s *return
/// value* is not one: the crate returns the deleted count, the BIF returns `true` —
/// see [`DELETE_ORACLE_RETURN`].)
#[rustfmt::skip]
const DELETE_EXPECTED: &[(&str, &str)] = &[
    ("set.select_count", "3"), ("set.select_delete", "3"), ("set.after_select_delete", "[1,5]"),
    ("set.match_delete_count", "1"), ("set.after_match_delete", "[1]"),
    ("set.objs.size", "1"), ("set.objs.select_delete_all", "1"), ("set.objs.size_after", "0"),
    ("ordered_set.select_count", "3"), ("ordered_set.select_delete", "3"), ("ordered_set.after_select_delete", "[1,5]"),
    ("ordered_set.match_delete_count", "1"), ("ordered_set.after_match_delete", "[1]"),
    ("ordered_set.objs.size", "1"), ("ordered_set.objs.select_delete_all", "1"), ("ordered_set.objs.size_after", "0"),
    ("bag.select_count", "3"), ("bag.select_delete", "3"), ("bag.after_select_delete", "[1,5]"),
    ("bag.match_delete_count", "1"), ("bag.after_match_delete", "[1]"),
    ("bag.objs.size", "3"), ("bag.objs.select_delete_all", "3"), ("bag.objs.size_after", "0"),
    ("duplicate_bag.select_count", "3"), ("duplicate_bag.select_delete", "3"), ("duplicate_bag.after_select_delete", "[1,5]"),
    ("duplicate_bag.match_delete_count", "1"), ("duplicate_bag.after_match_delete", "[1]"),
    ("duplicate_bag.objs.size", "3"), ("duplicate_bag.objs.select_delete_all", "3"), ("duplicate_bag.objs.size_after", "0"),
];

/// The oracle-only facts: the atom `ets:match_delete/2` returns for every kind
/// (`true`). The crate's `Table::match_delete` returns the deleted **count**
/// instead, so this is deliberately **not** a shared fact — it is checked only
/// against the live oracle.
#[rustfmt::skip]
const DELETE_ORACLE_RETURN: &[(&str, &str)] = &[
    ("set.match_delete_ret", "true"),
    ("ordered_set.match_delete_ret", "true"),
    ("bag.match_delete_ret", "true"),
    ("duplicate_bag.match_delete_ret", "true"),
];

/// The crate's delete rules, pinned to the recorded oracle figures (no oracle
/// needed).
#[test]
fn select_delete_and_select_count_match_the_recorded_oracle_figures() {
    let facts = all_delete_facts();
    for (key, want) in DELETE_EXPECTED {
        assert_eq!(facts.get(*key).map(String::as_str), Some(*want), "fact `{key}`");
    }
    assert_eq!(
        facts.len(),
        DELETE_EXPECTED.len(),
        "the battery produced a different number of facts than recorded"
    );
}

/// The oracle program of the delete battery (see [`DELETE_EXPECTED`] and
/// [`DELETE_ORACLE_RETURN`]).
const DELETE_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~p~n", [K, V]) end,
    Objs = [{N, N * 10} || N <- lists:seq(1, 5)],
    Guard = [{{'$1','_'}, [{'>','$1',1},{'<','$1',5}], [true]}],
    AllTrue = [{{'$1','$2'}, [], [true]}],
    F = fun(Kind) ->
        KS = atom_to_list(Kind),
        T = ets:new(del, [Kind]),
        [ets:insert(T, O) || O <- Objs],
        P(KS ++ ".select_count", ets:select_count(T, Guard)),
        P(KS ++ ".select_delete", ets:select_delete(T, Guard)),
        P(KS ++ ".after_select_delete", lists:sort([K || {K,_} <- ets:tab2list(T)])),
        P(KS ++ ".match_delete_count", length(ets:match_object(T, {5,'_'}))),
        P(KS ++ ".match_delete_ret", ets:match_delete(T, {5,'_'})),
        P(KS ++ ".after_match_delete", lists:sort([K || {K,_} <- ets:tab2list(T)])),
        ets:delete(T),
        M = ets:new(del2, [Kind]),
        [ets:insert(M, {1, V}) || V <- [10,20,30]],
        P(KS ++ ".objs.size", ets:info(M, size)),
        P(KS ++ ".objs.select_delete_all", ets:select_delete(M, AllTrue)),
        P(KS ++ ".objs.size_after", ets:info(M, size)),
        ets:delete(M)
    end,
    [F(K) || K <- [set, ordered_set, bag, duplicate_bag]],
    ok.
"#;

/// The live cross-check of the delete battery: the system `erl` runs the identical
/// script and both sides must reproduce every recorded figure — including the
/// `ets:match_delete/2` **return** (`true`, [`DELETE_ORACLE_RETURN`]) the crate's
/// count-returning API deliberately does not produce.
///
/// Skips loudly when no Erlang is installed; `BEAM_ETS_REQUIRE_ORACLE=1` turns the
/// skip into a failure.
#[test]
fn select_delete_and_select_count_agree_with_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "select_delete_and_select_count_agree_with_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        DELETE_ORACLE_PROGRAM,
        &[],
        "order_delete",
        Duration::from_secs(60),
    );
    let live = parse_lines(&String::from_utf8_lossy(&output.stdout));
    let facts = all_delete_facts();

    for (key, want) in DELETE_EXPECTED {
        assert_eq!(
            live.get(*key).map(String::as_str),
            Some(*want),
            "the oracle disagrees with the recorded figure for `{key}`"
        );
        assert_eq!(
            facts.get(*key).map(String::as_str),
            Some(*want),
            "the crate disagrees with the recorded figure for `{key}`"
        );
    }
    // The oracle's own `ets:match_delete/2` return is the atom `true`.
    for (key, want) in DELETE_ORACLE_RETURN {
        assert_eq!(
            live.get(*key).map(String::as_str),
            Some(*want),
            "the oracle's `ets:match_delete/2` return for `{key}`"
        );
    }
    assert_eq!(
        live.len(),
        DELETE_EXPECTED.len() + DELETE_ORACLE_RETURN.len(),
        "the oracle printed a different number of facts than recorded:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!(
        "[ets] delete family: {} shared + {} oracle-only facts agree with the live oracle",
        DELETE_EXPECTED.len(),
        DELETE_ORACLE_RETURN.len()
    );
}