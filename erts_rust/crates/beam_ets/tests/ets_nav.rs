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

//! Navigation (`first`/`next`/`last`/`prev`), `take/2`, `lookup_element/4` and the
//! view-layer traversal ORDER.
//!
//! The edge semantics below mirror the system `erl` (OTP 29 / erts-17.1), verified
//! with `erl -noshell -eval '...'`:
//!
//!   * an empty table: `first`/`last` return `'$end_of_table'` for every kind;
//!     `next`/`prev` return `'$end_of_table'` for `ordered_set` but raise `badarg`
//!     for the hash kinds (their walk is anchored at a present entry);
//!   * an absent anchor key: `ordered_set` treats it as a range bound (the nearest
//!     stored key above/below it) while the hash kinds raise `badarg`;
//!   * `next` past the largest stored key is `'$end_of_table'` for every kind.
//!
//! The guard-scoped view (`TableRead::{iter_keys, iter_keys_rev, iter_objects,
//! iter_objects_rev, foldl, foldr, tab2list_into}`) is a re-expression of
//! `ets:first/1` + `ets:next/2`, so its traversal order is pinned here too: every
//! forward form visits exactly the objects `tab2list` yields, in that order, and its
//! `*_rev`/`foldr` mirror is the exact reverse (`ordered_set`: ascending arithmetic
//! key order; hash kinds: unspecified, compared as a normalised multiset).

mod common;

use beam_common::BeamError;
use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions, TableTerm, TermStore};
use beam_term::prelude::*;
use common::*;
use std::collections::BTreeMap;
use std::time::Duration;

#[test]
fn first_and_last_are_none_for_every_kind_on_an_empty_table() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let t = new_table(kind);
        assert_eq!(t.first(&atoms), None, "{kind:?} empty first");
        assert_eq!(t.last(&atoms), None, "{kind:?} empty last");
    }
}

#[test]
fn next_and_prev_on_an_empty_table_depend_on_the_kind() {
    let atoms = AtomTable::new();
    // `ordered_set` reads the key as a range bound, so an empty table is simply
    // `'$end_of_table'` — not an error.
    let ordered = new_table(TableKind::OrderedSet);
    assert_eq!(ordered.next(make_small(5), &atoms).unwrap(), None);
    assert_eq!(ordered.prev(make_small(5), &atoms).unwrap(), None);

    // The hash kinds anchor the walk at a present entry, so every key of an empty
    // table is unknown and `ets:next/2` raises.
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let t = new_table(kind);
        assert!(
            matches!(t.next(make_small(5), &atoms), Err(BeamError::BadArg)),
            "{kind:?} empty next"
        );
        assert!(
            matches!(t.prev(make_small(5), &atoms), Err(BeamError::BadArg)),
            "{kind:?} empty prev"
        );
    }
}

#[test]
fn ordered_set_navigation_is_a_cmp_ordered_range_scan() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::OrderedSet);
    for k in [3i64, 1, 2] {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    assert_eq!(t.first(&atoms), Some(make_small(1)));
    assert_eq!(t.last(&atoms), Some(make_small(3)));

    // Present keys: the strict successor/predecessor, with `'$end_of_table'` at the
    // two ends.
    assert_eq!(t.next(make_small(1), &atoms).unwrap(), Some(make_small(2)));
    assert_eq!(t.next(make_small(2), &atoms).unwrap(), Some(make_small(3)));
    assert_eq!(t.next(make_small(3), &atoms).unwrap(), None);
    assert_eq!(t.prev(make_small(3), &atoms).unwrap(), Some(make_small(2)));
    assert_eq!(t.prev(make_small(2), &atoms).unwrap(), Some(make_small(1)));
    assert_eq!(t.prev(make_small(1), &atoms).unwrap(), None);

    // Absent keys are range bounds, never errors.
    assert_eq!(t.next(make_small(0), &atoms).unwrap(), Some(make_small(1)));
    assert_eq!(t.next(make_small(99), &atoms).unwrap(), None);
    assert_eq!(t.prev(make_small(99), &atoms).unwrap(), Some(make_small(3)));
    assert_eq!(t.prev(make_small(0), &atoms).unwrap(), None);

    // Deleting everything empties the navigation again.
    t.delete_all_objects();
    assert_eq!(t.first(&atoms), None);
    assert_eq!(t.last(&atoms), None);
    assert_eq!(t.next(make_small(1), &atoms).unwrap(), None);
    assert_eq!(t.prev(make_small(1), &atoms).unwrap(), None);
}

#[test]
fn hash_navigation_walks_every_key_once_and_rejects_an_absent_anchor() {
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);
        for k in [3i64, 1, 2] {
            t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
        }

        // The hash order is internal (bucket order), so only the *set* of visited
        // keys is contract: `first` + `next` visits each stored key exactly once.
        let mut walk = Vec::new();
        let mut cursor = t.first(&atoms);
        while let Some(key) = cursor {
            walk.push(key);
            cursor = t.next(key, &atoms).unwrap();
        }
        assert_eq!(walk.len(), 3, "{kind:?} walk length");
        for k in [1i64, 2, 3] {
            assert!(
                walk.iter()
                    .any(|&visited| eq(visited, make_small(k), &atoms)),
                "{kind:?} walk covers key {k}"
            );
        }

        // `last` closes the same order, and `prev(first)` is the start of it.
        let first = t.first(&atoms).unwrap();
        assert_eq!(t.last(&atoms), walk.last().copied(), "{kind:?} last");
        assert_eq!(t.prev(first, &atoms).unwrap(), None, "{kind:?} prev(first)");
        let last = t.last(&atoms).unwrap();
        assert_eq!(t.next(last, &atoms).unwrap(), None, "{kind:?} next(last)");

        // An anchor key that is not stored is `badarg` (verified `erl` behaviour).
        assert!(matches!(
            t.next(make_small(0), &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.next(make_small(99), &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.prev(make_small(0), &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.prev(make_small(99), &atoms),
            Err(BeamError::BadArg)
        ));

        // A multi kind reports one key per entry, so a re-walk of a `bag` whose key
        // holds several objects still visits that key once.
        if kind.is_multi() {
            t.insert(object(&mut heap, 1, 11), &atoms).unwrap();
            let mut second_walk = Vec::new();
            let mut cursor = t.first(&atoms);
            while let Some(key) = cursor {
                second_walk.push(key);
                cursor = t.next(key, &atoms).unwrap();
            }
            assert_eq!(second_walk.len(), 3, "{kind:?} multi-object walk");
        }
    }
}

/// The hash kinds navigate in an **unspecified internal order**: the ETS contract
/// fixes *which* keys are reachable and how an absent anchor is rejected, never the
/// order. What is contract — and is pinned here — is:
///
///   * `first`/`last` return a **present** key (never a bogus one) or
///     `'$end_of_table'`;
///   * the `first` + `next` walk visits every stored key exactly once, and `last`
///     closes that same walk (`prev(first)` and `next(last)` are `'$end_of_table'`);
///   * `next`/`prev` on an **absent** anchor key are `badarg` — the walk is anchored
///     at a present entry.
///
/// Rationalised against the real `erl` (OTP 29 / erts-17.1): a 3-key `set` answers
/// `first = last = 1` there, but that is the *internal* order, not a rule — so this
/// test deliberately asserts nothing of the form `last == first`.
#[test]
fn hash_navigation_order_is_unspecified_but_every_returned_key_is_present() {
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(2048);
        let mut t = new_table(kind);

        const KEYS: i64 = 25;
        for k in 0..KEYS {
            t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
        }
        let present: Vec<Eterm> = (0..KEYS).map(make_small).collect();
        let is_present = |key: Eterm| present.iter().any(|&p| eq(p, key, &atoms));

        // A non-empty table has a first (present) key; an empty one does not
        // (`'$end_of_table'`) — covered by the empty-table test above.
        let first = t.first(&atoms).expect("a non-empty table has a first key");
        assert!(is_present(first), "{kind:?}: first is a stored key");

        // `first` + `next` visits every stored key exactly once.
        let mut walk = Vec::new();
        let mut cursor = Some(first);
        while let Some(key) = cursor {
            assert!(is_present(key), "{kind:?}: every walked key is stored");
            assert!(
                !walk.iter().any(|&seen| eq(seen, key, &atoms)),
                "{kind:?}: each stored key is visited exactly once"
            );
            walk.push(key);
            cursor = t.next(key, &atoms).unwrap();
        }
        assert_eq!(walk.len(), KEYS as usize, "{kind:?}: the walk covers every key");

        // `last` closes the same order; the two ends are `'$end_of_table'`.
        let last = t.last(&atoms).expect("a non-empty table has a last key");
        assert!(is_present(last), "{kind:?}: last is a stored key");
        assert!(
            eq(last, *walk.last().unwrap(), &atoms),
            "{kind:?}: last closes the walk"
        );
        assert_eq!(t.next(last, &atoms).unwrap(), None, "{kind:?}: next(last)");
        assert_eq!(t.prev(first, &atoms).unwrap(), None, "{kind:?}: prev(first)");

        // An absent anchor key is always `badarg`, whatever its value.
        for absent in [make_small(-1), make_small(KEYS), make_small(999)] {
            assert!(
                matches!(t.next(absent, &atoms), Err(BeamError::BadArg)),
                "{kind:?}: next on an absent key is badarg"
            );
            assert!(
                matches!(t.prev(absent, &atoms), Err(BeamError::BadArg)),
                "{kind:?}: prev on an absent key is badarg"
            );
        }
    }
}

#[test]
fn take_removes_and_returns_the_keys_objects_for_every_kind() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        // An absent key yields an empty list, not an error.
        assert!(t.take(make_small(9), &atoms).unwrap().is_empty());

        let a = object(&mut heap, 1, 10);
        t.insert(a, &atoms).unwrap();
        if kind.is_multi() {
            t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
        }

        let taken = t.take(make_small(1), &atoms).unwrap();
        let expected = if kind.is_multi() { 2 } else { 1 };
        assert_eq!(taken.len(), expected, "{kind:?} take count");
        assert!(
            taken.iter().any(|&o| eq(o, a, &atoms)),
            "{kind:?} take value"
        );
        assert!(!t.member(make_small(1), &atoms), "{kind:?} taken key gone");
        assert_eq!(t.size(), 0, "{kind:?} take empties the entry");

        // The entry really is gone: a second `take` is empty again.
        assert!(t.take(make_small(1), &atoms).unwrap().is_empty());

        // The table is still usable afterwards.
        t.insert(object(&mut heap, 2, 20), &atoms).unwrap();
        assert_eq!(t.size(), 1);
    }
}

#[test]
fn lookup_element_or_returns_the_default_only_for_an_absent_key() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        // {1, 10, 20}
        let o = heap
            .tuple(&[make_small(1), make_small(10), make_small(20)])
            .unwrap();
        t.insert(o, &atoms).unwrap();
        let default = interned_atom(&mut atoms, "default");

        // Present key: the element, `pos` being 1-based.
        assert_eq!(
            t.lookup_element_or(make_small(1), 1, default, &atoms)
                .unwrap(),
            make_small(1)
        );
        assert_eq!(
            t.lookup_element_or(make_small(1), 2, default, &atoms)
                .unwrap(),
            make_small(10)
        );
        assert_eq!(
            t.lookup_element_or(make_small(1), 3, default, &atoms)
                .unwrap(),
            make_small(20)
        );

        // Absent key: the default, whatever `pos` (verified `erl` behaviour).
        assert_eq!(
            t.lookup_element_or(make_small(9), 2, default, &atoms)
                .unwrap(),
            default
        );
        assert_eq!(
            t.lookup_element_or(make_small(9), 5, default, &atoms)
                .unwrap(),
            default
        );

        // `pos == 0` is `badarg` before the key is even consulted …
        assert!(matches!(
            t.lookup_element_or(make_small(9), 0, default, &atoms),
            Err(BeamError::BadArg)
        ));
        // … and a present key with an out-of-range `pos` stays `badarg`.
        assert!(matches!(
            t.lookup_element_or(make_small(1), 4, default, &atoms),
            Err(BeamError::BadArg)
        ));

        // The `/3` form keeps its old behaviour: `badarg` on an absent key.
        assert!(matches!(
            t.lookup_element(make_small(9), 2, &atoms),
            Err(BeamError::BadArg)
        ));
    }
}

// ---------------------------------------------------------------------------
// Absent anchors, cross-checked with the live oracle for all four kinds
// ---------------------------------------------------------------------------
//
// The navigation seam is split by kind (see `src/table/read.rs` `check_navigable`):
//
//   * an `ordered_set` reads `next`/`prev`'s key as a **range bound** — an absent
//     key is fine (the nearest stored key above/below it), and an empty table is
//     simply `'$end_of_table'`;
//   * the hash kinds (`set`/`bag`/`duplicate_bag`) anchor the walk at a *present*
//     entry, so an absent key — which includes every key of an empty table — is
//     `badarg`.
//
// `first`/`last` take no anchor and are `'$end_of_table'` on an empty table for
// every kind.

/// Render a `next`/`prev` result the way `erl`'s `~p` does: the key, `badarg` for an
/// absent anchor on a hash kind, or `'$end_of_table'`.
fn render_nav(result: beam_common::Result<Option<Eterm>>) -> String {
    match result {
        Ok(Some(key)) => signed_value(key).to_string(),
        Ok(None) => "'$end_of_table'".to_string(),
        Err(BeamError::BadArg) => "badarg".to_string(),
        Err(other) => panic!("an unexpected navigation error: {other:?}"),
    }
}

/// Render a `first`/`last` result the way `erl`'s `~p` does.
fn render_end(result: Option<Eterm>) -> String {
    match result {
        Some(key) => signed_value(key).to_string(),
        None => "'$end_of_table'".to_string(),
    }
}

/// The absent-anchor facts of one kind: a non-empty table (`{1,10}..{3,30}`) probed
/// with the absent anchors `0` and `99`, and an empty table's `first`/`last`/`next`/
/// `prev`.
fn nav_facts(kind: TableKind) -> BTreeMap<String, String> {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let name = kind.as_atom_name();
    let mut facts = BTreeMap::new();

    let mut table = new_table(kind);
    for key in 1..=3i64 {
        table.insert(object(&mut heap, key, key * 10), &atoms).unwrap();
    }
    facts.insert(
        format!("{name}.next_absent_lo"),
        render_nav(table.next(make_small(0), &atoms)),
    );
    facts.insert(
        format!("{name}.next_absent_hi"),
        render_nav(table.next(make_small(99), &atoms)),
    );
    facts.insert(
        format!("{name}.prev_absent_lo"),
        render_nav(table.prev(make_small(0), &atoms)),
    );
    facts.insert(
        format!("{name}.prev_absent_hi"),
        render_nav(table.prev(make_small(99), &atoms)),
    );

    let empty = new_table(kind);
    facts.insert(format!("{name}.empty_first"), render_end(empty.first(&atoms)));
    facts.insert(format!("{name}.empty_last"), render_end(empty.last(&atoms)));
    facts.insert(
        format!("{name}.empty_next"),
        render_nav(empty.next(make_small(5), &atoms)),
    );
    facts.insert(
        format!("{name}.empty_prev"),
        render_nav(empty.prev(make_small(5), &atoms)),
    );
    facts
}

/// Every absent-anchor fact the battery must produce — the figures printed by the
/// system `erl` (OTP 29.1.1 / erts-17.1) for the identical script.
#[rustfmt::skip]
const NAV_EXPECTED: &[(&str, &str)] = &[
    ("set.next_absent_lo", "badarg"), ("set.next_absent_hi", "badarg"),
    ("set.prev_absent_lo", "badarg"), ("set.prev_absent_hi", "badarg"),
    ("set.empty_first", "'$end_of_table'"), ("set.empty_last", "'$end_of_table'"),
    ("set.empty_next", "badarg"), ("set.empty_prev", "badarg"),
    ("ordered_set.next_absent_lo", "1"), ("ordered_set.next_absent_hi", "'$end_of_table'"),
    ("ordered_set.prev_absent_lo", "'$end_of_table'"), ("ordered_set.prev_absent_hi", "3"),
    ("ordered_set.empty_first", "'$end_of_table'"), ("ordered_set.empty_last", "'$end_of_table'"),
    ("ordered_set.empty_next", "'$end_of_table'"), ("ordered_set.empty_prev", "'$end_of_table'"),
    ("bag.next_absent_lo", "badarg"), ("bag.next_absent_hi", "badarg"),
    ("bag.prev_absent_lo", "badarg"), ("bag.prev_absent_hi", "badarg"),
    ("bag.empty_first", "'$end_of_table'"), ("bag.empty_last", "'$end_of_table'"),
    ("bag.empty_next", "badarg"), ("bag.empty_prev", "badarg"),
    ("duplicate_bag.next_absent_lo", "badarg"), ("duplicate_bag.next_absent_hi", "badarg"),
    ("duplicate_bag.prev_absent_lo", "badarg"), ("duplicate_bag.prev_absent_hi", "badarg"),
    ("duplicate_bag.empty_first", "'$end_of_table'"), ("duplicate_bag.empty_last", "'$end_of_table'"),
    ("duplicate_bag.empty_next", "badarg"), ("duplicate_bag.empty_prev", "badarg"),
];

/// The four kinds, in the battery's order.
const NAV_KINDS: [TableKind; 4] = [
    TableKind::Set,
    TableKind::OrderedSet,
    TableKind::Bag,
    TableKind::DuplicateBag,
];

/// Every fact of the absent-anchor battery.
fn all_nav_facts() -> BTreeMap<String, String> {
    let mut facts = BTreeMap::new();
    for kind in NAV_KINDS {
        facts.extend(nav_facts(kind));
    }
    facts
}

/// The crate's absent-anchor rules, pinned to the recorded oracle figures (no oracle
/// needed): an absent `next`/`prev` anchor is `badarg` on the hash kinds and a range
/// bound on an `ordered_set`, and an empty table is `'$end_of_table'` for
/// `first`/`last` on every kind.
#[test]
fn absent_anchors_match_the_recorded_oracle_figures() {
    let facts = all_nav_facts();
    for (key, want) in NAV_EXPECTED {
        assert_eq!(facts.get(*key).map(String::as_str), Some(*want), "fact `{key}`");
    }
    assert_eq!(
        facts.len(),
        NAV_EXPECTED.len(),
        "the battery produced a different number of facts than recorded"
    );
}

/// The oracle program of the absent-anchor battery (see [`NAV_EXPECTED`]); it catches
/// `badarg` so a raised error is a printed fact rather than a crash.
const NAV_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~p~n", [K, V]) end,
    G = fun(F) -> try {ok, F()} catch error:badarg -> badarg end end,
    R = fun({ok, V}) -> V; (badarg) -> badarg end,
    F = fun(Kind) ->
        KS = atom_to_list(Kind),
        T = ets:new(nav, [Kind]),
        [ets:insert(T, {N, N * 10}) || N <- [1, 2, 3]],
        P(KS ++ ".next_absent_lo", R(G(fun() -> ets:next(T, 0) end))),
        P(KS ++ ".next_absent_hi", R(G(fun() -> ets:next(T, 99) end))),
        P(KS ++ ".prev_absent_lo", R(G(fun() -> ets:prev(T, 0) end))),
        P(KS ++ ".prev_absent_hi", R(G(fun() -> ets:prev(T, 99) end))),
        ets:delete(T),
        E = ets:new(nav2, [Kind]),
        P(KS ++ ".empty_first", R(G(fun() -> ets:first(E) end))),
        P(KS ++ ".empty_last", R(G(fun() -> ets:last(E) end))),
        P(KS ++ ".empty_next", R(G(fun() -> ets:next(E, 5) end))),
        P(KS ++ ".empty_prev", R(G(fun() -> ets:prev(E, 5) end))),
        ets:delete(E)
    end,
    [F(K) || K <- [set, ordered_set, bag, duplicate_bag]],
    ok.
"#;

/// The live cross-check of the absent-anchor rules: the system `erl` runs the
/// identical probes and both sides must reproduce every recorded figure.
///
/// The oracle is **required by default**, so a missing Erlang fails this check
/// rather than skipping it; `BEAM_ETS_ALLOW_NO_ORACLE=1` opts out explicitly.
#[test]
fn absent_anchors_agree_with_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "absent_anchors_agree_with_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        NAV_ORACLE_PROGRAM,
        &[],
        "order_nav",
        Duration::from_secs(60),
    );
    let live = parse_lines(&String::from_utf8_lossy(&output.stdout));
    let facts = all_nav_facts();

    for (key, want) in NAV_EXPECTED {
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
    assert_eq!(
        live.len(),
        NAV_EXPECTED.len(),
        "the oracle printed a different number of facts than recorded:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!(
        "[ets] absent anchors: {} facts agree with the live oracle",
        NAV_EXPECTED.len()
    );
}
// ---------------------------------------------------------------------------
// The view-layer traversal ORDER (`TableRead::{iter_keys, iter_objects, foldl,
// foldr, tab2list_into}`), for all four kinds
// ---------------------------------------------------------------------------
//
// The guard-scoped view is a re-expression of `ets:first/1` + `ets:next/2`: every
// iterator walks the interior in the table's own iteration order, so a view
// traversal visits exactly the objects `tab2list` yields, in exactly that order, and
// its `*_rev` / `foldr` forms are the exact mirror. For an `ordered_set` that order
// is the ascending arithmetic key order (oracle-exact); for the hash kinds it is the
// interior's unspecified order, so the live cross-check compares the normalised
// (sorted) rendering.

/// A public `SharedTable` of `kind` (the tests drive the guard-scoped views).
fn shared_table(kind: TableKind) -> SharedTable {
    let mut opts = TableOptions::new();
    opts.set_kind(kind).set_access(Access::Public);
    SharedTable::new(Table::new(TableId::next(), opts).expect("a fresh table"))
}

/// Insert the `{1,10}..{5,50}` objects into `shared` as its owner would.
fn fill(shared: &SharedTable, atoms: &AtomTable) {
    let mut source = Heap::new(64);
    for key in 1..=5i64 {
        let object = object(&mut source, key, key * 10);
        shared
            .with_write(|view| view.insert(object, atoms))
            .expect("no poisoned lock")
            .expect("a supported object inserts");
    }
}

/// The `(key, value)` of a yielded [`TableTerm`] (extracted under the guard).
fn term_pair(object: &TableTerm<'_>) -> (i64, i64) {
    let key = object
        .tuple_element(0)
        .and_then(|element| element.as_small_int())
        .expect("every stored key element is a small integer");
    let value = object
        .tuple_element(1)
        .and_then(|element| element.as_small_int())
        .expect("every stored value element is a small integer");
    (key, value)
}

/// The keys [`TableRead::iter_keys`] yields, as owned integers.
fn view_keys(shared: &SharedTable, atoms: &AtomTable) -> Vec<i64> {
    shared
        .with_read(|view| {
            view.iter_keys(atoms)
                .filter_map(|key| key.as_small_int())
                .collect()
        })
        .expect("no poisoned lock")
}

/// The keys [`TableRead::iter_keys_rev`] yields, as owned integers.
fn view_keys_rev(shared: &SharedTable, atoms: &AtomTable) -> Vec<i64> {
    shared
        .with_read(|view| {
            view.iter_keys_rev(atoms)
                .filter_map(|key| key.as_small_int())
                .collect()
        })
        .expect("no poisoned lock")
}

/// The objects [`TableRead::iter_objects`] yields, as `(key, value)` pairs.
fn view_objects(shared: &SharedTable, atoms: &AtomTable) -> Vec<(i64, i64)> {
    shared
        .with_read(|view| view.iter_objects(atoms).map(|o| term_pair(&o)).collect())
        .expect("no poisoned lock")
}

/// The objects [`TableRead::iter_objects_rev`] yields, as `(key, value)` pairs.
fn view_objects_rev(shared: &SharedTable, atoms: &AtomTable) -> Vec<(i64, i64)> {
    shared
        .with_read(|view| {
            view.iter_objects_rev(atoms)
                .map(|o| term_pair(&o))
                .collect()
        })
        .expect("no poisoned lock")
}

/// [`TableRead::tab2list`]'s objects, as `(key, value)` pairs.
fn view_tab2list(shared: &SharedTable) -> Vec<(i64, i64)> {
    shared
        .with_read(|view| view.tab2list().iter().map(|o| term_pair(o)).collect())
        .expect("no poisoned lock")
}

/// [`TableRead::foldl`]'s visit order over the objects, as `(key, value)` pairs.
fn view_foldl(shared: &SharedTable, atoms: &AtomTable) -> Vec<(i64, i64)> {
    shared
        .with_read(|view| {
            view.foldl(
                Vec::new(),
                |mut acc, object| {
                    acc.push(term_pair(&object));
                    acc
                },
                atoms,
            )
        })
        .expect("no poisoned lock")
}

/// [`TableRead::foldr`]'s visit order over the objects, as `(key, value)` pairs.
fn view_foldr(shared: &SharedTable, atoms: &AtomTable) -> Vec<(i64, i64)> {
    shared
        .with_read(|view| {
            view.foldr(
                Vec::new(),
                |object, mut acc| {
                    acc.push(term_pair(&object));
                    acc
                },
                atoms,
            )
        })
        .expect("no poisoned lock")
}

/// The order [`TableRead::tab2list_into`] copies the objects out in, as `(key,
/// value)` pairs (read from the caller-owned copies).
fn view_tab2list_into(shared: &SharedTable) -> Vec<(i64, i64)> {
    let mut dest = TermStore::with_default_allocator();
    let owned = shared
        .with_read(|view| view.tab2list_into(&mut dest))
        .expect("no poisoned lock")
        .expect("every stored object is copyable");
    owned.iter().map(|object| pair_of(*object)).collect()
}

/// Every view-layer traversal visits the objects in exactly `tab2list` order, and
/// the `*_rev` / `foldr` forms are the exact mirror — for **all four kinds**. An
/// `ordered_set` is the ascending key order.
#[test]
fn view_layer_traverses_in_tab2list_order_for_every_kind() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let shared = shared_table(kind);
        fill(&shared, &atoms);

        let reference = view_tab2list(&shared);
        let keys: Vec<i64> = reference.iter().map(|(key, _)| *key).collect();

        assert_eq!(view_keys(&shared, &atoms), keys, "{kind:?}: iter_keys order");
        assert_eq!(
            view_objects(&shared, &atoms),
            reference,
            "{kind:?}: iter_objects order"
        );
        assert_eq!(view_foldl(&shared, &atoms), reference, "{kind:?}: foldl order");
        assert_eq!(
            view_tab2list_into(&shared),
            reference,
            "{kind:?}: tab2list_into order"
        );

        let mut rev_keys = keys.clone();
        rev_keys.reverse();
        let mut rev_objects = reference.clone();
        rev_objects.reverse();
        assert_eq!(
            view_keys_rev(&shared, &atoms),
            rev_keys,
            "{kind:?}: iter_keys_rev order"
        );
        assert_eq!(
            view_objects_rev(&shared, &atoms),
            rev_objects,
            "{kind:?}: iter_objects_rev order"
        );
        assert_eq!(
            view_foldr(&shared, &atoms),
            rev_objects,
            "{kind:?}: foldr order"
        );

        if kind.is_ordered() {
            assert_eq!(keys, vec![1, 2, 3, 4, 5], "{kind:?}: ascending key order");
        }
    }
}

/// The view-order facts of one kind: the `first`/`next` walk's keys and its objects.
/// A hash kind's order is unspecified, so its sequences are **sorted**.
fn view_facts(kind: TableKind) -> BTreeMap<String, String> {
    let atoms = AtomTable::new();
    let shared = shared_table(kind);
    fill(&shared, &atoms);
    let name = kind.as_atom_name();

    let mut keys = view_keys(&shared, &atoms);
    let mut objects = view_objects(&shared, &atoms);
    if !kind.is_ordered() {
        keys.sort();
        objects.sort();
    }
    let mut facts = BTreeMap::new();
    facts.insert(format!("{name}.walk_keys"), render_ints(&keys));
    facts.insert(format!("{name}.walk_objects"), render_pairs(&objects));
    facts
}

/// The view-order facts the battery must produce — the figures printed by the system
/// `erl` for the identical `ets:first/next` walk (recorded, so it also runs with no
/// `erl` on `$PATH`).
#[rustfmt::skip]
const VIEW_EXPECTED: &[(&str, &str)] = &[
    ("set.walk_keys", "[1,2,3,4,5]"), ("set.walk_objects", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("ordered_set.walk_keys", "[1,2,3,4,5]"), ("ordered_set.walk_objects", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("bag.walk_keys", "[1,2,3,4,5]"), ("bag.walk_objects", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("duplicate_bag.walk_keys", "[1,2,3,4,5]"), ("duplicate_bag.walk_objects", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
];

/// The four kinds, in the battery's order.
const VIEW_KINDS: [TableKind; 4] = [
    TableKind::Set,
    TableKind::OrderedSet,
    TableKind::Bag,
    TableKind::DuplicateBag,
];

/// Every fact of the view-order battery.
fn all_view_facts() -> BTreeMap<String, String> {
    let mut facts = BTreeMap::new();
    for kind in VIEW_KINDS {
        facts.extend(view_facts(kind));
    }
    facts
}

/// The crate's view-order rules, pinned to the recorded figures (no oracle needed).
#[test]
fn view_order_matches_the_recorded_figures() {
    let facts = all_view_facts();
    for (key, want) in VIEW_EXPECTED {
        assert_eq!(facts.get(*key).map(String::as_str), Some(*want), "fact `{key}`");
    }
    assert_eq!(facts.len(), VIEW_EXPECTED.len(), "fact count");
}

/// The oracle program: an `ordered_set`'s exact `ets:first/1` + `ets:next/2` walk,
/// and its `ets:tab2list` objects, per kind (`norm/2` sorts a hash kind, whose order
/// ERTS leaves unspecified).
const VIEW_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~p~n", [K, V]) end,
    Norm = fun(ordered_set, L) -> L; (_, L) -> lists:sort(L) end,
    F = fun(Kind) ->
        KS = atom_to_list(Kind),
        T = ets:new(view, [Kind]),
        [ets:insert(T, {N, N * 10}) || N <- lists:seq(1, 5)],
        P(KS ++ ".walk_keys", Norm(Kind, walk_keys(T))),
        P(KS ++ ".walk_objects", Norm(Kind, ets:tab2list(T))),
        ets:delete(T)
    end,
    [F(K) || K <- [set, ordered_set, bag, duplicate_bag]],
    ok.

walk_keys(T) -> walk_keys(ets:first(T), T, []).
walk_keys('$end_of_table', _T, Acc) -> lists:reverse(Acc);
walk_keys(Key, T, Acc) -> walk_keys(ets:next(T, Key), T, [Key | Acc]).
"#;

/// The live cross-check of the view order: the crate's `iter_keys` (an
/// `ets:first`+`ets:next` walk) and `iter_objects` must reproduce the same normalised
/// sequences the system `erl` prints for the identical walk. The oracle is
/// **required by default**, so a missing Erlang fails this check rather than skipping
/// it; `BEAM_ETS_ALLOW_NO_ORACLE=1` opts out explicitly.
#[test]
fn view_order_agrees_with_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "view_order_agrees_with_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        VIEW_ORACLE_PROGRAM,
        &[],
        "order_view",
        Duration::from_secs(60),
    );
    let live = parse_lines(&String::from_utf8_lossy(&output.stdout));
    let facts = all_view_facts();

    for (key, want) in VIEW_EXPECTED {
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
    assert_eq!(
        live.len(),
        VIEW_EXPECTED.len(),
        "the oracle printed a different number of facts than recorded:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!(
        "[ets] view order: {} facts agree with the live oracle",
        VIEW_EXPECTED.len()
    );
}
