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

//! A third broadened slice of real system stdlib coverage: the **balanced-tree
//! collections** `gb_trees` and `gb_sets`, loaded straight from the running
//! Erlang's stdlib (`<stdlib>/ebin/<module>.beam`, OTP 29 / `erts-17.1`, the
//! differential oracle) and run on a fresh [`beam_engine::Machine`] over the
//! shared heap/atom host in `support`.
//!
//! `tests/stdlib_more.rs` already proves the *base* collection API of these two
//! modules (`empty`/`insert`/`lookup`/`get`/`delete`/`keys`/`values`/`size`/
//! `to_list`/`is_empty` for `gb_trees`; `empty`/`add`/`del_element`/`from_list`/
//! `is_element`/`is_subset`/`size`/`smallest`/`largest`/`to_list` for `gb_sets`).
//! This suite adds the **still-unpinned** exports — every one deliberately
//! disjoint from that base set:
//!
//! * **`gb_trees`** — `from_orddict/1`, `is_defined/2`, `update/3`, `take/2`,
//!   `take_smallest/1`, `take_largest/1`, `smallest/1`, `largest/1`,
//!   `iterator/1` + `next/1` (an in-order walk), `balance/1`;
//! * **`gb_sets`** — `union/2`, `intersection/2`, `subtract/2`, `singleton/1`,
//!   `is_equal/2` (both branches — the differing branch throws `not_equal`, which
//!   the host floor's `erlang:raise/3` turns into `false`), `take_smallest/1`,
//!   `take_largest/1`.
//!
//! ## Why these two modules run
//!
//! A balanced tree computes a node's height with integer bit shifts
//! (`erlang:bsl/2`/`bsr/2`) and picks the taller of two subtrees with the
//! term-ordering `erlang:max/2`; both are served by the host's BIF floor
//! (`support/bifs.rs`), so the whole `gb_trees`/`gb_sets` surface — including the
//! `gb_sets` set algebra, which is built on `gb_trees` — executes end to end. The
//! opcode families reached are the arithmetic `gc_bif` pair (`bsl`/`bsr`),
//! term-ordering `max`/`min`, the tuple readers (`element`), the cons/tuple
//! builders and the local/external call surface.
//!
//! Every expected value is the **real** system ERTS result, derived in one
//! batched oracle run (see the `oracle:` note beside each assertion), e.g.
//! `erl -noshell -eval 'io:format("~p~n",[gb_trees:to_list(gb_trees:from_orddict \
//! ([{1,10},{2,20}]))]),halt().'` ⇒ `[{1,10},{2,20}]`. The stdlib is located at
//! **test time**; when `erl`/stdlib is unavailable every test **skips loudly**.

mod support;

use beam_engine::terms;
use beam_term::{make_small, Eterm};
use support::{
    assert_atom, assert_int_list, assert_int_pair_list, assert_small, assert_term, int_list,
    skip_reason, stdlib_ebin, term_atom, term_list, term_tuple, ModuleHost,
};

/// A host over the system stdlib `ebin`.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    ModuleHost::new(ebin)
}

/// Skips the calling test (loudly) when the oracle stdlib is unavailable;
/// returns `true` when the caller should return early.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

/// Asserts `got` is the atom `name`, interning the name first so the assertion
/// never depends on some *other* module having happened to mention it.
fn assert_shared_atom(host: &mut ModuleHost, got: Eterm, name: &str) {
    term_atom(host, name);
    assert_atom(host, got, name);
}

/// Builds `[{K,V}, …]` — the orddict shape `gb_trees:from_orddict/1` consumes.
fn int_pairs(host: &mut ModuleHost, pairs: &[(i64, i64)]) -> Eterm {
    let elems: Vec<Eterm> = pairs
        .iter()
        .map(|&(k, v)| term_tuple(host, &[make_small(k), make_small(v)]))
        .collect();
    term_list(host, &elems)
}

/// The shared `{1,10},{2,20}` tree every `gb_trees` case here starts from —
/// the exact oracle expression `gb_trees:from_orddict([{1,10},{2,20}])`.
fn two_tree(host: &mut ModuleHost) -> Eterm {
    let pairs = int_pairs(host, &[(1, 10), (2, 20)]);
    host.call("gb_trees", "from_orddict", &[pairs])
        .expect("gb_trees:from_orddict/1 must run")
}

/// `gb_trees:to_list/1` of `tree`, asserted against the oracle's `{Key,Value}`
/// pairs (a tree's internal shape is opaque, so every tree result is pinned
/// through this canonical in-order view).
fn assert_tree(host: &mut ModuleHost, tree: Eterm, want: &[(i64, i64)]) {
    let listed = host
        .call("gb_trees", "to_list", &[tree])
        .expect("gb_trees:to_list/1 must run");
    assert_int_pair_list(host, listed, want);
}

/// `gb_sets:to_list/1` of `set`, asserted against the oracle's ascending ints.
fn assert_set(host: &mut ModuleHost, set: Eterm, want: &[i64]) {
    let listed = host
        .call("gb_sets", "to_list", &[set])
        .expect("gb_sets:to_list/1 must run");
    assert_int_list(host, listed, want);
}

// ---------------------------------------------------------------------------
// `gb_trees` — the balanced binary search tree
// ---------------------------------------------------------------------------

#[test]
fn stdlib_gb_trees_from_orddict_is_defined_and_update() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: gb_trees:to_list(gb_trees:from_orddict([{1,10},{2,20}]))
    //         = [{1,10},{2,20}]
    let t = two_tree(&mut host);
    assert_tree(&mut host, t, &[(1, 10), (2, 20)]);

    // oracle: gb_trees:is_defined(1, T) = true ; gb_trees:is_defined(9, T) = false
    let hit = host
        .call("gb_trees", "is_defined", &[make_small(1), t])
        .expect("gb_trees:is_defined/2 must run");
    assert_shared_atom(&mut host, hit, "true");
    let miss = host
        .call("gb_trees", "is_defined", &[make_small(9), t])
        .expect("gb_trees:is_defined/2 must run");
    assert_shared_atom(&mut host, miss, "false");

    // oracle: gb_trees:to_list(gb_trees:update(1, 99, T)) = [{1,99},{2,20}]
    // (`update/3` replaces an **existing** key's value in a fresh tree.)
    let updated = host
        .call("gb_trees", "update", &[make_small(1), make_small(99), t])
        .expect("gb_trees:update/3 must run");
    assert_tree(&mut host, updated, &[(1, 99), (2, 20)]);

    // oracle: gb_trees:to_list(T) = [{1,10},{2,20}] — the update did NOT mutate
    // the source tree (`gb_trees` is persistent, no in-place write).
    assert_tree(&mut host, t, &[(1, 10), (2, 20)]);
}

#[test]
fn stdlib_gb_trees_take_and_take_extremes() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let t = two_tree(&mut host);

    // oracle: gb_trees:take(1, T) = {10, {1,{2,20,nil,nil}}}
    //         gb_trees:to_list(element(2, gb_trees:take(1, T))) = [{2,20}]
    let taken = host
        .call("gb_trees", "take", &[make_small(1), t])
        .expect("gb_trees:take/2 must run");
    assert_small(
        &host,
        terms::tuple_element(taken, 0).expect("take value"),
        10,
    );
    let rest = terms::tuple_element(taken, 1).expect("take tree");
    assert_tree(&mut host, rest, &[(2, 20)]);

    // oracle: gb_trees:take_smallest(T) = {1, 10, {1,{2,20,nil,nil}}}
    //         to_list(element(3, …)) = [{2,20}]
    let smallest = host
        .call("gb_trees", "take_smallest", &[t])
        .expect("gb_trees:take_smallest/1 must run");
    assert_small(
        &host,
        terms::tuple_element(smallest, 0).expect("take_smallest key"),
        1,
    );
    assert_small(
        &host,
        terms::tuple_element(smallest, 1).expect("take_smallest value"),
        10,
    );
    let rest = terms::tuple_element(smallest, 2).expect("take_smallest tree");
    assert_tree(&mut host, rest, &[(2, 20)]);

    // oracle: gb_trees:take_largest(T) = {2, 20, {1,{1,10,nil,nil}}}
    //         to_list(element(3, …)) = [{1,10}]
    let largest = host
        .call("gb_trees", "take_largest", &[t])
        .expect("gb_trees:take_largest/1 must run");
    assert_small(
        &host,
        terms::tuple_element(largest, 0).expect("take_largest key"),
        2,
    );
    assert_small(
        &host,
        terms::tuple_element(largest, 1).expect("take_largest value"),
        20,
    );
    let rest = terms::tuple_element(largest, 2).expect("take_largest tree");
    assert_tree(&mut host, rest, &[(1, 10)]);
}

#[test]
fn stdlib_gb_trees_smallest_largest_and_balance() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let t = two_tree(&mut host);

    // oracle: gb_trees:smallest(T) = {1,10}
    let smallest = host
        .call("gb_trees", "smallest", &[t])
        .expect("gb_trees:smallest/1 must run");
    let want = term_tuple(&mut host, &[make_small(1), make_small(10)]);
    assert_term(&host, smallest, want);

    // oracle: gb_trees:largest(T) = {2,20}
    let largest = host
        .call("gb_trees", "largest", &[t])
        .expect("gb_trees:largest/1 must run");
    let want = term_tuple(&mut host, &[make_small(2), make_small(20)]);
    assert_term(&host, largest, want);

    // oracle: gb_trees:to_list(gb_trees:balance(T)) = [{1,10},{2,20}]
    let balanced = host
        .call("gb_trees", "balance", &[t])
        .expect("gb_trees:balance/1 must run");
    assert_tree(&mut host, balanced, &[(1, 10), (2, 20)]);
}

#[test]
fn stdlib_gb_trees_iterator_next_walks_in_order() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let t = two_tree(&mut host);

    let iter = host
        .call("gb_trees", "iterator", &[t])
        .expect("gb_trees:iterator/1 must run");

    // oracle: element(1, gb_trees:next(gb_trees:iterator(T))) = 1
    //         element(2, gb_trees:next(gb_trees:iterator(T))) = 10
    let n1 = host
        .call("gb_trees", "next", &[iter])
        .expect("gb_trees:next/1 must run");
    assert_small(&host, terms::tuple_element(n1, 0).expect("next key"), 1);
    assert_small(&host, terms::tuple_element(n1, 1).expect("next value"), 10);
    let iter = terms::tuple_element(n1, 2).expect("next iterator");

    // oracle: element(1, gb_trees:next(<iter2>)) = 2 (in ascending key order)
    let n2 = host
        .call("gb_trees", "next", &[iter])
        .expect("gb_trees:next/1 must run");
    assert_small(&host, terms::tuple_element(n2, 0).expect("next key"), 2);
    assert_small(&host, terms::tuple_element(n2, 1).expect("next value"), 20);
    let iter = terms::tuple_element(n2, 2).expect("next iterator");

    // oracle: gb_trees:next(<iter3>) = none (the walk is exhausted)
    let done = host
        .call("gb_trees", "next", &[iter])
        .expect("gb_trees:next/1 must run");
    let none = term_atom(&mut host, "none");
    assert_term(&host, done, none);
}

// ---------------------------------------------------------------------------
// `gb_sets` — the balanced-tree set (its set algebra is built on `gb_trees`)
// ---------------------------------------------------------------------------

#[test]
fn stdlib_gb_sets_union_intersection_subtract_is_equal() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let a = int_list(&mut host, &[1, 2, 3]);
    let s = host
        .call("gb_sets", "from_list", &[a])
        .expect("gb_sets:from_list/1 must run");
    let b = int_list(&mut host, &[2, 3, 4]);
    let s12 = host
        .call("gb_sets", "from_list", &[b])
        .expect("gb_sets:from_list/1 must run");

    // oracle: gb_sets:to_list(gb_sets:union(from_list([1,2,3]), from_list([2,3,4])))
    //         = [1,2,3,4]
    let union = host
        .call("gb_sets", "union", &[s, s12])
        .expect("gb_sets:union/2 must run");
    assert_set(&mut host, union, &[1, 2, 3, 4]);

    // oracle: gb_sets:to_list(gb_sets:intersection(…)) = [2,3]
    let inter = host
        .call("gb_sets", "intersection", &[s, s12])
        .expect("gb_sets:intersection/2 must run");
    assert_set(&mut host, inter, &[2, 3]);

    // oracle: gb_sets:to_list(gb_sets:subtract(from_list([1,2,3]), from_list([2,3,4])))
    //         = [1]
    let diff = host
        .call("gb_sets", "subtract", &[s, s12])
        .expect("gb_sets:subtract/2 must run");
    assert_set(&mut host, diff, &[1]);

    // oracle: gb_sets:is_equal(from_list([1,2,3]), from_list([1,2,3])) = true
    let c = int_list(&mut host, &[1, 2, 3]);
    let same = host
        .call("gb_sets", "from_list", &[c])
        .expect("gb_sets:from_list/1 must run");
    let equal = host
        .call("gb_sets", "is_equal", &[s, same])
        .expect("gb_sets:is_equal/2 must run");
    assert_shared_atom(&mut host, equal, "true");
}

/// `gb_sets:is_equal/2` on two **differing** sets — the branch that throws.
///
/// `gb_sets:is_equal/2`'s first clause (`gb_sets.erl:222`) walks the second set's
/// sorted keys while recursing the first and, the moment a key differs, runs
/// `throw(not_equal)`, relying on its own
/// `try is_equal_1(…) of [] -> true catch throw:not_equal -> false end` to turn
/// that throw into `false`. The compiler lowers `throw(not_equal)` to
/// `erlang:raise(throw, not_equal, nil)` (`gb_sets.erl:240`), and the host floor's
/// `erlang:raise/3` raises class `throw` with the atom `not_equal`, which the
/// `catch throw:not_equal` clause matches — so the walk returns `false` exactly as
/// the oracle does.
#[test]
fn stdlib_gb_sets_is_equal_on_differing_sets_is_false() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let a = int_list(&mut host, &[1, 2, 3]);
    let s = host
        .call("gb_sets", "from_list", &[a])
        .expect("gb_sets:from_list/1 must run");
    let b = int_list(&mut host, &[2, 3, 4]);
    let s12 = host
        .call("gb_sets", "from_list", &[b])
        .expect("gb_sets:from_list/1 must run");

    // oracle: erl -noshell -eval 'io:format("~w~n",[gb_sets:is_equal(gb_sets:from_list([1,2,3]), gb_sets:from_list([2,3,4]))]),halt().'
    //         => false
    let equal = host
        .call("gb_sets", "is_equal", &[s, s12])
        .expect("gb_sets:is_equal/2 must run");
    assert_shared_atom(&mut host, equal, "false");
}

#[test]
fn stdlib_gb_sets_singleton_and_take_extremes() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: gb_sets:to_list(gb_sets:singleton(5)) = [5]
    let single = host
        .call("gb_sets", "singleton", &[make_small(5)])
        .expect("gb_sets:singleton/1 must run");
    assert_set(&mut host, single, &[5]);

    let l = int_list(&mut host, &[1, 2, 3]);
    let s = host
        .call("gb_sets", "from_list", &[l])
        .expect("gb_sets:from_list/1 must run");

    // oracle: gb_sets:take_smallest(from_list([1,2,3])) = {1, {2,{2,nil,{3,nil,nil}}}}
    //         to_list(element(2, …)) = [2,3]
    let smallest = host
        .call("gb_sets", "take_smallest", &[s])
        .expect("gb_sets:take_smallest/1 must run");
    assert_small(
        &host,
        terms::tuple_element(smallest, 0).expect("take_smallest elem"),
        1,
    );
    let rest = terms::tuple_element(smallest, 1).expect("take_smallest set");
    assert_set(&mut host, rest, &[2, 3]);

    // oracle: gb_sets:take_largest(from_list([1,2,3])) = {3, {2,{2,{1,nil,nil},nil}}}
    //         to_list(element(2, …)) = [1,2]
    let largest = host
        .call("gb_sets", "take_largest", &[s])
        .expect("gb_sets:take_largest/1 must run");
    assert_small(
        &host,
        terms::tuple_element(largest, 0).expect("take_largest elem"),
        3,
    );
    let rest = terms::tuple_element(largest, 1).expect("take_largest set");
    assert_set(&mut host, rest, &[1, 2]);
}
