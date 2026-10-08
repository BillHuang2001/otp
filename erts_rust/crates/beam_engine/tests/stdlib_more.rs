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

//! A **second** broadened slice of real system stdlib coverage: the widened
//! typed-IR interpreter runs whole additional **system stdlib** modules, not just
//! the `lists`/`ordsets`/`proplists` trio of `tests/stdlib_lists.rs`.
//!
//! Every module here is loaded straight from the running Erlang's stdlib
//! (`<stdlib>/ebin/<module>.beam`, OTP 29 / `erts-17.1`, the differential
//! oracle) and its exports are run on a fresh [`beam_engine::Machine`] over the
//! shared heap/atom host in `support`.
//!
//! New modules proven end to end:
//!
//! * **`orddict`** — an association list kept sorted (`store`/`find`/`fetch`/
//!   `erase`/`size`/`is_key`/`is_empty`) plus the fun forms `map`/`filter`/
//!   `update`/`fold`;
//! * **`queue`** — the two-list FIFO (`new`/`in`/`out`/`peek`/`len`/`to_list`/
//!   `from_list`/`is_empty`/`reverse`/`member`, and `filter`/`all` with a fun);
//! * **`gb_trees`** — a balanced binary search tree (`empty`/`insert`/`lookup`/
//!   `get`/`delete`/`keys`/`values`/`size`/`to_list`/`is_empty`);
//! * **`gb_sets`** — a balanced-tree set (`empty`/`add`/`del_element`/`from_list`/
//!   `is_element`/`is_subset`/`size`/`smallest`/`largest`/`to_list`);
//! * a handful more `lists`, `ordsets` and `proplists` exports; and
//! * the **`stdlib_more_harness`** fixture, whose wrappers reach the
//!   **`lists:member/2` NIF** (whose `.beam` body is the `erlang:nif_error/1`
//!   stub) and pass anonymous funs into `lists`/`orddict`/`queue`.
//!
//! `gb_trees`/`gb_sets` are the reason the host's BIF floor gained integer bit
//! shifts (`bsl`/`bsr`) and the term-ordering `max`/`min`: a balanced tree
//! computes its node heights with them. The `lists`/`queue` fun forms exercised
//! here also pinned a real bug in the host's `lists:reverse/2` emulation (it
//! returned a multi-element list unchanged); that is fixed in `support`.
//!
//! Every expected value is the **real** system ERTS result, cross-checked with
//! `erl` (see the `oracle` notes in the test bodies). The stdlib is located at
//! **test time**; when `erl`/stdlib is unavailable every test **skips loudly**.

mod support;

use beam_engine::terms;
use beam_term::{make_small, Eterm};
use support::{
    assert_atom, assert_int_list, assert_int_pair_list, assert_int_queue, assert_keyed_list,
    assert_small, assert_term, int_list, keyed_tuple, skip_reason, stdlib_ebin, term_atom,
    term_list, term_tuple, ModuleHost,
};

/// The committed second-harness fixture (a real `.beam` from the system `erlc`).
const HARNESS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/stdlib_more_harness.beam"
);

/// A host over the system stdlib `ebin` with the second harness registered.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    host.register("stdlib_more_harness", HARNESS_BEAM);
    host
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

/// Builds the sorted `{a, 1}, {b, 2}` orddict/proplist the `orddict` and
/// `proplists` tests share.
fn ab_pairs(host: &mut ModuleHost) -> Eterm {
    let a = keyed_tuple(host, "a", 1);
    let b = keyed_tuple(host, "b", 2);
    term_list(host, &[a, b])
}

// ---------------------------------------------------------------------------
// `orddict` — a new real stdlib module
// ---------------------------------------------------------------------------

#[test]
fn stdlib_orddict_builds_queries_and_erases() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: orddict:new() = []
    let empty = host.call("orddict", "new", &[]).expect("orddict:new/0 must run");
    assert!(terms::list_elements(empty).expect("a list").is_empty());

    // oracle: orddict:store(a,1,new()) = [{a,1}]
    let a = host.intern("a");
    let d1 = host
        .call("orddict", "store", &[a, make_small(1), empty])
        .expect("orddict:store/3 must run");
    assert_keyed_list(&host, d1, &[("a", 1)]);

    // oracle: ... store(b,2,...) = [{a,1},{b,2}]
    let b = host.intern("b");
    let d2 = host
        .call("orddict", "store", &[b, make_small(2), d1])
        .expect("orddict:store/3 must run");
    assert_keyed_list(&host, d2, &[("a", 1), ("b", 2)]);

    // oracle: orddict:find(a, [{a,1},{b,2}]) = {ok,1}
    let found = host
        .call("orddict", "find", &[a, d2])
        .expect("orddict:find/2 must run");
    let ok = term_atom(&mut host, "ok");
    let want = term_tuple(&mut host, &[ok, make_small(1)]);
    assert_term(&host, found, want);

    // oracle: orddict:find(z, ...) = error
    let z = host.intern("z");
    let miss = host
        .call("orddict", "find", &[z, d2])
        .expect("orddict:find/2 must run");
    let error = term_atom(&mut host, "error");
    assert_term(&host, miss, error);

    // oracle: orddict:fetch(a, ...) = 1 ; orddict:size(...) = 2
    let fetched = host
        .call("orddict", "fetch", &[a, d2])
        .expect("orddict:fetch/2 must run");
    assert_small(&host, fetched, 1);
    let size = host
        .call("orddict", "size", &[d2])
        .expect("orddict:size/1 must run");
    assert_small(&host, size, 2);

    // oracle: orddict:is_key(b, ...) = true ; orddict:is_empty([]) = true
    let is_key = host
        .call("orddict", "is_key", &[b, d2])
        .expect("orddict:is_key/2 must run");
    assert_shared_atom(&mut host, is_key, "true");
    let is_empty = host
        .call("orddict", "is_empty", &[empty])
        .expect("orddict:is_empty/1 must run");
    assert_shared_atom(&mut host, is_empty, "true");

    // oracle: orddict:erase(a, [{a,1},{b,2}]) = [{b,2}]
    let erased = host
        .call("orddict", "erase", &[a, d2])
        .expect("orddict:erase/2 must run");
    assert_keyed_list(&host, erased, &[("b", 2)]);
}

#[test]
fn stdlib_orddict_fun_forms_via_harness() {
    // The fun-taking `orddict` exports (`map`/`filter`/`update`/`fold`) are only
    // reachable through a caller that can build an anonymous fun, so the harness
    // picks one closure per function and this drives the cross-module `call_fun`
    // path into real `orddict`.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let dict = ab_pairs(&mut host);

    // oracle: orddict:map(fun(_K,V) -> V*10 end, [{a,1},{b,2}]) = [{a,10},{b,20}]
    let mapped = host
        .call("stdlib_more_harness", "orddict_map", &[dict])
        .expect("harness orddict_map/1 must run");
    assert_keyed_list(&host, mapped, &[("a", 10), ("b", 20)]);

    // oracle: orddict:filter(fun(_K,V) -> V>1 end, [{a,1},{b,2}]) = [{b,2}]
    let filtered = host
        .call("stdlib_more_harness", "orddict_filter", &[dict])
        .expect("harness orddict_filter/1 must run");
    assert_keyed_list(&host, filtered, &[("b", 2)]);

    // oracle: orddict:update(a, fun(V) -> V+1 end, [{a,1},{b,2}]) = [{a,2},{b,2}]
    let updated = host
        .call("stdlib_more_harness", "orddict_update", &[dict])
        .expect("harness orddict_update/1 must run");
    assert_keyed_list(&host, updated, &[("a", 2), ("b", 2)]);

    // oracle: orddict:fold(fun(_K,V,Acc) -> V+Acc end, 0, [{a,1},{b,2}]) = 3
    let folded = host
        .call("stdlib_more_harness", "orddict_fold", &[dict])
        .expect("harness orddict_fold/1 must run");
    assert_small(&host, folded, 3);
}

// ---------------------------------------------------------------------------
// `queue` — a second new real stdlib module (a two-list FIFO)
// ---------------------------------------------------------------------------

#[test]
fn stdlib_queue_fifo_round_trip() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: queue:new() = {[],[]}
    let q0 = host.call("queue", "new", &[]).expect("queue:new/0 must run");
    assert_int_queue(&host, q0, &[], &[]);

    // oracle: queue:in(1, queue:new()) = {[1],[]}
    let q1 = host
        .call("queue", "in", &[make_small(1), q0])
        .expect("queue:in/2 must run");
    assert_int_queue(&host, q1, &[1], &[]);

    // oracle: queue:in(2, queue:in(1, queue:new())) = {[2],[1]}
    let q2 = host
        .call("queue", "in", &[make_small(2), q1])
        .expect("queue:in/2 must run");
    assert_int_queue(&host, q2, &[2], &[1]);

    // oracle: queue:to_list(...) = [1,2] ; queue:len(...) = 2
    let list = host
        .call("queue", "to_list", &[q2])
        .expect("queue:to_list/1 must run");
    assert_int_list(&host, list, &[1, 2]);
    let len = host
        .call("queue", "len", &[q2])
        .expect("queue:len/1 must run");
    assert_small(&host, len, 2);

    // oracle: queue:peek(...) = {value,1}
    let peek = host
        .call("queue", "peek", &[q2])
        .expect("queue:peek/1 must run");
    let value = term_atom(&mut host, "value");
    let want_peek = term_tuple(&mut host, &[value, make_small(1)]);
    assert_term(&host, peek, want_peek);

    // oracle: queue:out(...) = {{value,1},{[],[2]}}
    let out = host
        .call("queue", "out", &[q2])
        .expect("queue:out/1 must run");
    let value = term_atom(&mut host, "value");
    let head = term_tuple(&mut host, &[value, make_small(1)]);
    let rest = {
        let rear = term_list(&mut host, &[]);
        let front = term_list(&mut host, &[make_small(2)]);
        term_tuple(&mut host, &[rear, front])
    };
    let want_out = term_tuple(&mut host, &[head, rest]);
    assert_term(&host, out, want_out);

    // oracle: queue:member(2, ...) = true (the `lists:member/2` NIF path)
    let member = host
        .call("queue", "member", &[make_small(2), q2])
        .expect("queue:member/2 must run");
    assert_shared_atom(&mut host, member, "true");

    // oracle: queue:is_empty(...) = false ; queue:reverse(...) = {[1],[2]}
    let is_empty = host
        .call("queue", "is_empty", &[q2])
        .expect("queue:is_empty/1 must run");
    assert_shared_atom(&mut host, is_empty, "false");
    let reversed = host
        .call("queue", "reverse", &[q2])
        .expect("queue:reverse/1 must run");
    assert_int_queue(&host, reversed, &[1], &[2]);
}

#[test]
fn stdlib_queue_from_list_matches_the_real_layout() {
    // `queue:from_list/1` for three or more elements calls the `lists:split/2` +
    // `lists:reverse/2` pair; the asserted internal `{Rear,Front}` shape is the
    // exact oracle output, so this pins both the split and the reverse.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: queue:from_list([1,2,3]) = {[3,2],[1]}
    let l3 = int_list(&mut host, &[1, 2, 3]);
    let q3 = host
        .call("queue", "from_list", &[l3])
        .expect("queue:from_list/1 must run");
    assert_int_queue(&host, q3, &[3, 2], &[1]);
    let back3 = host
        .call("queue", "to_list", &[q3])
        .expect("queue:to_list/1 must run");
    assert_int_list(&host, back3, &[1, 2, 3]);

    // oracle: queue:from_list([1,2,3,4]) = {[4,3],[1,2]}
    let l4 = int_list(&mut host, &[1, 2, 3, 4]);
    let q4 = host
        .call("queue", "from_list", &[l4])
        .expect("queue:from_list/1 must run");
    assert_int_queue(&host, q4, &[4, 3], &[1, 2]);
    let back4 = host
        .call("queue", "to_list", &[q4])
        .expect("queue:to_list/1 must run");
    assert_int_list(&host, back4, &[1, 2, 3, 4]);
}

#[test]
fn stdlib_queue_fun_forms_via_harness() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: queue:filter(fun(X) -> X>2 end, queue:from_list([1,2,3,4])) = {[4],[3]}
    let l = int_list(&mut host, &[1, 2, 3, 4]);
    let q = host
        .call("queue", "from_list", &[l])
        .expect("queue:from_list/1 must run");
    let filtered = host
        .call("stdlib_more_harness", "queue_filter_gt", &[q])
        .expect("harness queue_filter_gt/1 must run");
    assert_int_queue(&host, filtered, &[4], &[3]);
    let filtered_list = host
        .call("queue", "to_list", &[filtered])
        .expect("queue:to_list/1 must run");
    assert_int_list(&host, filtered_list, &[3, 4]);

    // oracle: queue:all(fun(X) -> X>0 end, queue:from_list([1,2,3])) = true
    let l3 = int_list(&mut host, &[1, 2, 3]);
    let q3 = host
        .call("queue", "from_list", &[l3])
        .expect("queue:from_list/1 must run");
    let all = host
        .call("stdlib_more_harness", "queue_all_pos", &[q3])
        .expect("harness queue_all_pos/1 must run");
    assert_shared_atom(&mut host, all, "true");
}

// ---------------------------------------------------------------------------
// `gb_trees` — a third new module (balanced binary search tree)
// ---------------------------------------------------------------------------

#[test]
fn stdlib_gb_trees_insert_lookup_and_in_order_traversal() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let empty = host
        .call("gb_trees", "empty", &[])
        .expect("gb_trees:empty/0 must run");
    let t = host
        .call("gb_trees", "insert", &[make_small(3), make_small(30), empty])
        .expect("gb_trees:insert/3 must run");
    let t = host
        .call("gb_trees", "insert", &[make_small(1), make_small(10), t])
        .expect("gb_trees:insert/3 must run");
    let t = host
        .call("gb_trees", "insert", &[make_small(2), make_small(20), t])
        .expect("gb_trees:insert/3 must run");

    // oracle: gb_trees:to_list(T) = [{1,10},{2,20},{3,30}] ; size = 3
    let list = host
        .call("gb_trees", "to_list", &[t])
        .expect("gb_trees:to_list/1 must run");
    assert_int_pair_list(&host, list, &[(1, 10), (2, 20), (3, 30)]);
    let size = host
        .call("gb_trees", "size", &[t])
        .expect("gb_trees:size/1 must run");
    assert_small(&host, size, 3);

    // oracle: gb_trees:get(2, T) = 20 ; keys(T) = [1,2,3] ; values(T) = [10,20,30]
    let got = host
        .call("gb_trees", "get", &[make_small(2), t])
        .expect("gb_trees:get/2 must run");
    assert_small(&host, got, 20);
    let keys = host
        .call("gb_trees", "keys", &[t])
        .expect("gb_trees:keys/1 must run");
    assert_int_list(&host, keys, &[1, 2, 3]);
    let values = host
        .call("gb_trees", "values", &[t])
        .expect("gb_trees:values/1 must run");
    assert_int_list(&host, values, &[10, 20, 30]);

    // oracle: gb_trees:lookup(3, T) = {value,30} ; lookup(9, T) = none
    let lookup = host
        .call("gb_trees", "lookup", &[make_small(3), t])
        .expect("gb_trees:lookup/2 must run");
    let value = term_atom(&mut host, "value");
    let want = term_tuple(&mut host, &[value, make_small(30)]);
    assert_term(&host, lookup, want);
    let miss = host
        .call("gb_trees", "lookup", &[make_small(9), t])
        .expect("gb_trees:lookup/2 must run");
    let none = term_atom(&mut host, "none");
    assert_term(&host, miss, none);

    // oracle: gb_trees:is_empty(T) = false ;
    //         to_list(delete(2, T)) = [{1,10},{3,30}]
    let is_empty = host
        .call("gb_trees", "is_empty", &[t])
        .expect("gb_trees:is_empty/1 must run");
    assert_shared_atom(&mut host, is_empty, "false");
    let deleted = host
        .call("gb_trees", "delete", &[make_small(2), t])
        .expect("gb_trees:delete/2 must run");
    let after = host
        .call("gb_trees", "to_list", &[deleted])
        .expect("gb_trees:to_list/1 must run");
    assert_int_pair_list(&host, after, &[(1, 10), (3, 30)]);
}

// ---------------------------------------------------------------------------
// `gb_sets` — a fourth new module (balanced-tree set)
// ---------------------------------------------------------------------------

#[test]
fn stdlib_gb_sets_membership_bounds_and_set_ops() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let empty = host
        .call("gb_sets", "empty", &[])
        .expect("gb_sets:empty/0 must run");
    let s = host
        .call("gb_sets", "add", &[make_small(5), empty])
        .expect("gb_sets:add/2 must run");
    let s = host
        .call("gb_sets", "add", &[make_small(2), s])
        .expect("gb_sets:add/2 must run");

    // oracle: gb_sets:to_list(S) = [2,5] ; size = 2 ; smallest = 2 ; largest = 5
    let list = host
        .call("gb_sets", "to_list", &[s])
        .expect("gb_sets:to_list/1 must run");
    assert_int_list(&host, list, &[2, 5]);
    let size = host
        .call("gb_sets", "size", &[s])
        .expect("gb_sets:size/1 must run");
    assert_small(&host, size, 2);
    let smallest = host
        .call("gb_sets", "smallest", &[s])
        .expect("gb_sets:smallest/1 must run");
    assert_small(&host, smallest, 2);
    let largest = host
        .call("gb_sets", "largest", &[s])
        .expect("gb_sets:largest/1 must run");
    assert_small(&host, largest, 5);

    // oracle: gb_sets:is_element(2, S) = true ; is_element(9, S) = false
    let hit = host
        .call("gb_sets", "is_element", &[make_small(2), s])
        .expect("gb_sets:is_element/2 must run");
    assert_shared_atom(&mut host, hit, "true");
    let miss = host
        .call("gb_sets", "is_element", &[make_small(9), s])
        .expect("gb_sets:is_element/2 must run");
    assert_shared_atom(&mut host, miss, "false");

    // oracle: to_list(from_list([3,1,2])) = [1,2,3]
    let l = int_list(&mut host, &[3, 1, 2]);
    let from = host
        .call("gb_sets", "from_list", &[l])
        .expect("gb_sets:from_list/1 must run");
    let from_list = host
        .call("gb_sets", "to_list", &[from])
        .expect("gb_sets:to_list/1 must run");
    assert_int_list(&host, from_list, &[1, 2, 3]);

    // oracle: to_list(del_element(2, from_list([1,2,3]))) = [1,3]
    let removed = host
        .call("gb_sets", "del_element", &[make_small(2), from])
        .expect("gb_sets:del_element/2 must run");
    let removed_list = host
        .call("gb_sets", "to_list", &[removed])
        .expect("gb_sets:to_list/1 must run");
    assert_int_list(&host, removed_list, &[1, 3]);

    // oracle: is_subset(from_list([1,2]), from_list([1,2,3])) = true
    let la = int_list(&mut host, &[1, 2]);
    let a = host
        .call("gb_sets", "from_list", &[la])
        .expect("gb_sets:from_list/1 must run");
    let lb = int_list(&mut host, &[1, 2, 3]);
    let b = host
        .call("gb_sets", "from_list", &[lb])
        .expect("gb_sets:from_list/1 must run");
    let subset = host
        .call("gb_sets", "is_subset", &[a, b])
        .expect("gb_sets:is_subset/2 must run");
    assert_shared_atom(&mut host, subset, "true");
}

// ---------------------------------------------------------------------------
// More `lists` / `ordsets` / `proplists` exports
// ---------------------------------------------------------------------------

#[test]
fn stdlib_lists_more_pure_functions() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let l = int_list(&mut host, &[3, 1, 3, 2]);
    let r = host.call("lists", "usort", &[l]).expect("lists:usort/1 must run");
    assert_int_list(&host, r, &[1, 2, 3]);

    let l = int_list(&mut host, &[1, 2, 3]);
    let r = host.call("lists", "sum", &[l]).expect("lists:sum/1 must run");
    assert_small(&host, r, 6);

    let l = int_list(&mut host, &[5, 2, 9]);
    let r = host.call("lists", "max", &[l]).expect("lists:max/1 must run");
    assert_small(&host, r, 9);

    let l = int_list(&mut host, &[5, 2, 9]);
    let r = host.call("lists", "min", &[l]).expect("lists:min/1 must run");
    assert_small(&host, r, 2);

    let r = host
        .call("lists", "duplicate", &[make_small(3), make_small(7)])
        .expect("lists:duplicate/2 must run");
    assert_int_list(&host, r, &[7, 7, 7]);

    let l = int_list(&mut host, &[10, 20, 30]);
    let r = host.call("lists", "nth", &[make_small(2), l]).expect("lists:nth/2 must run");
    assert_small(&host, r, 20);

    let l = int_list(&mut host, &[10, 20, 30]);
    let r = host.call("lists", "last", &[l]).expect("lists:last/1 must run");
    assert_small(&host, r, 30);

    let l = int_list(&mut host, &[1, 2, 3]);
    let r = host
        .call("lists", "droplast", &[l])
        .expect("lists:droplast/1 must run");
    assert_int_list(&host, r, &[1, 2]);

    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[3, 4]);
    let r = host.call("lists", "zip", &[a, b]).expect("lists:zip/2 must run");
    assert_int_pair_list(&host, r, &[(1, 3), (2, 4)]);

    let l = int_list(&mut host, &[1, 2, 3]);
    let r = host
        .call("lists", "delete", &[make_small(2), l])
        .expect("lists:delete/2 must run");
    assert_int_list(&host, r, &[1, 3]);

    // `lists:subtract/2` reaches the `erlang:'--'/2` BIF.
    let l = int_list(&mut host, &[1, 2, 3]);
    let rem = int_list(&mut host, &[2]);
    let r = host
        .call("lists", "subtract", &[l, rem])
        .expect("lists:subtract/2 must run");
    assert_int_list(&host, r, &[1, 3]);

    let a = int_list(&mut host, &[1, 3]);
    let b = int_list(&mut host, &[2, 4]);
    let r = host
        .call("lists", "umerge", &[a, b])
        .expect("lists:umerge/2 must run");
    assert_int_list(&host, r, &[1, 2, 3, 4]);

    let r = host
        .call("lists", "seq", &[make_small(1), make_small(10), make_small(3)])
        .expect("lists:seq/3 must run");
    assert_int_list(&host, r, &[1, 4, 7, 10]);

    let inner_a = int_list(&mut host, &[1, 2]);
    let inner_b = int_list(&mut host, &[3]);
    let l = term_list(&mut host, &[inner_a, inner_b]);
    let r = host
        .call("lists", "flatten", &[l])
        .expect("lists:flatten/1 must run");
    assert_int_list(&host, r, &[1, 2, 3]);
}

#[test]
fn stdlib_lists_reverse_nif_via_harness() {
    // `lists:reverse/2` is a NIF too (`lists.beam`'s body is the
    // `erlang:nif_error/1` stub), so it is entered through the harness wrapper —
    // exactly as `queue:from_list/1` enters it. A multi-element list with a
    // non-empty tail is the case an earlier host emulation got wrong (it returned
    // the list unchanged).
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: lists:reverse([1,2,3],[9,8]) = [3,2,1,9,8]
    let l = int_list(&mut host, &[1, 2, 3]);
    let tail = int_list(&mut host, &[9, 8]);
    let r = host
        .call("stdlib_more_harness", "reverse", &[l, tail])
        .expect("harness reverse/2 must run");
    assert_int_list(&host, r, &[3, 2, 1, 9, 8]);

    // oracle: lists:reverse([1,2,3],[]) = [3,2,1]
    let l = int_list(&mut host, &[1, 2, 3]);
    let empty = term_list(&mut host, &[]);
    let r = host
        .call("stdlib_more_harness", "reverse", &[l, empty])
        .expect("harness reverse/2 must run");
    assert_int_list(&host, r, &[3, 2, 1]);
}

#[test]
fn stdlib_ordsets_more_set_operations() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: ordsets:add_element(3, [1,2]) = [1,2,3]
    let l = int_list(&mut host, &[1, 2]);
    let added = host
        .call("ordsets", "add_element", &[make_small(3), l])
        .expect("ordsets:add_element/2 must run");
    assert_int_list(&host, added, &[1, 2, 3]);

    // oracle: ordsets:del_element(2, [1,2,3]) = [1,3]
    let l = int_list(&mut host, &[1, 2, 3]);
    let removed = host
        .call("ordsets", "del_element", &[make_small(2), l])
        .expect("ordsets:del_element/2 must run");
    assert_int_list(&host, removed, &[1, 3]);

    // oracle: ordsets:size([1,2,3]) = 3
    let l = int_list(&mut host, &[1, 2, 3]);
    let size = host
        .call("ordsets", "size", &[l])
        .expect("ordsets:size/1 must run");
    assert_small(&host, size, 3);

    // oracle: ordsets:is_subset([1,2], [1,2,3]) = true
    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[1, 2, 3]);
    let subset = host
        .call("ordsets", "is_subset", &[a, b])
        .expect("ordsets:is_subset/2 must run");
    assert_shared_atom(&mut host, subset, "true");

    // oracle: ordsets:is_disjoint([1,2], [3,4]) = true
    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[3, 4]);
    let disjoint = host
        .call("ordsets", "is_disjoint", &[a, b])
        .expect("ordsets:is_disjoint/2 must run");
    assert_shared_atom(&mut host, disjoint, "true");
}

#[test]
fn stdlib_proplists_lookup_property_defined_and_delete() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let props = ab_pairs(&mut host);
    let key_a = host.intern("a");

    // oracle: proplists:lookup(a, Props) = {a,1}
    let lookup = host
        .call("proplists", "lookup", &[key_a, props])
        .expect("proplists:lookup/2 must run");
    let a = term_atom(&mut host, "a");
    let want = term_tuple(&mut host, &[a, make_small(1)]);
    assert_term(&host, lookup, want);

    // oracle: proplists:property(a, Props) = {a, [{a,1},{b,2}]}
    let prop = host
        .call("proplists", "property", &[key_a, props])
        .expect("proplists:property/2 must run");
    let a = term_atom(&mut host, "a");
    let want = term_tuple(&mut host, &[a, props]);
    assert_term(&host, prop, want);

    // oracle: proplists:is_defined(a, Props) = true
    let defined = host
        .call("proplists", "is_defined", &[key_a, props])
        .expect("proplists:is_defined/2 must run");
    assert_shared_atom(&mut host, defined, "true");

    // oracle: proplists:delete(a, Props) = [{b,2}]
    let deleted = host
        .call("proplists", "delete", &[key_a, props])
        .expect("proplists:delete/2 must run");
    assert_keyed_list(&host, deleted, &[("b", 2)]);
}

// ---------------------------------------------------------------------------
// The harness wrappers for `lists` (the `member/2` NIF and the fun forms)
// ---------------------------------------------------------------------------

#[test]
fn stdlib_lists_member_nif_via_harness() {
    // `lists:member/2` is a NIF (`lists.beam`'s body is the `erlang:nif_error/1`
    // stub), so it cannot be entered as a direct export — the harness wrapper is
    // the caller, exactly as `queue:member/2` is for `queue`.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let l = int_list(&mut host, &[10, 20, 30]);

    // oracle: lists:member(20, [10,20,30]) = true
    let hit = host
        .call("stdlib_more_harness", "member", &[make_small(20), l])
        .expect("harness member/2 must run");
    assert_shared_atom(&mut host, hit, "true");

    // oracle: lists:member(99, [10,20,30]) = false  (exact `=:=` semantics)
    let miss = host
        .call("stdlib_more_harness", "member", &[make_small(99), l])
        .expect("harness member/2 must run");
    assert_shared_atom(&mut host, miss, "false");
}

#[test]
fn stdlib_lists_fun_forms_via_harness() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: lists:partition(fun(X) -> X>1 end, [1,2,3]) = {[2,3],[1]}
    let l = int_list(&mut host, &[1, 2, 3]);
    let partition = host
        .call("stdlib_more_harness", "lists_partition", &[l])
        .expect("harness lists_partition/1 must run");
    let yes = int_list(&mut host, &[2, 3]);
    let no = int_list(&mut host, &[1]);
    let want = term_tuple(&mut host, &[yes, no]);
    assert_term(&host, partition, want);

    // oracle: lists:splitwith(fun(X) -> X<3 end, [1,2,3,1]) = {[1,2],[3,1]}
    let l = int_list(&mut host, &[1, 2, 3, 1]);
    let splitwith = host
        .call("stdlib_more_harness", "lists_splitwith", &[l])
        .expect("harness lists_splitwith/1 must run");
    let taken = int_list(&mut host, &[1, 2]);
    let rest = int_list(&mut host, &[3, 1]);
    let want = term_tuple(&mut host, &[taken, rest]);
    assert_term(&host, splitwith, want);

    // oracle: lists:sort(fun(A,B) -> A>=B end, [1,3,2]) = [3,2,1]
    let l = int_list(&mut host, &[1, 3, 2]);
    let sorted = host
        .call("stdlib_more_harness", "lists_sort_desc", &[l])
        .expect("harness lists_sort_desc/1 must run");
    assert_int_list(&host, sorted, &[3, 2, 1]);

    // oracle: lists:foldl(fun(X,Acc) -> X+Acc end, 0, [1,2,3,4]) = 10
    let l = int_list(&mut host, &[1, 2, 3, 4]);
    let sum = host
        .call("stdlib_more_harness", "lists_foldl_sum", &[l])
        .expect("harness lists_foldl_sum/1 must run");
    assert_small(&host, sum, 10);

    // oracle: lists:zipwith(fun(X,Y) -> X+Y end, [1,2], [3,4]) = [4,6]
    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[3, 4]);
    let zipped = host
        .call("stdlib_more_harness", "lists_zipwith_add", &[a, b])
        .expect("harness lists_zipwith_add/2 must run");
    assert_int_list(&host, zipped, &[4, 6]);
}

// ---------------------------------------------------------------------------
// Robustness guard (does not depend on the oracle)
// ---------------------------------------------------------------------------

#[test]
fn skip_reason_and_stdlib_ebin_agree() {
    // Mirrors `tests/stdlib_lists.rs`: whenever a skip reason exists, `stdlib_ebin`
    // must be `None`, and vice versa — so the skip guard never masks a usable
    // stdlib.
    assert_eq!(skip_reason().is_some(), stdlib_ebin().is_none());
}
