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
//! guard-truth) plus `match_delete`.

mod common;

use beam_ets::{compile, TableKind};
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
