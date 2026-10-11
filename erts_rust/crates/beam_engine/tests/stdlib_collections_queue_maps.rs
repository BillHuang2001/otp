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

//! **`queue` / `maps` collections slice** — oracle-checked end-to-end evidence
//! for two more real installed-OTP modules run through the typed-IR interpreter.
//!
//! Both are loaded straight from the running Erlang's stdlib
//! (`<stdlib>/ebin/<module>.beam`, OTP 29 / `erts-17.1`, the differential oracle)
//! and their exports run on a fresh [`beam_engine::Machine`] over the shared
//! heap/atom host in `support`. Every expected value is the **real** system ERTS
//! result, reproduced with `erl -noshell -eval …` and quoted in an `// oracle:`
//! comment beside its assertion — nothing is guessed — and every test **skips
//! loudly** when the oracle stdlib is unavailable.
//!
//! * **`queue`** — the two-list FIFO. This slice covers the inserters the existing
//!   suites omit (`cons/2`, `in_r/2`, `snoc/2`), the rear accessors (`peek_r/1`,
//!   `get_r/1`, `last/1`, `out_r/1`), the rear droppers (`init/1`, `liat/1`), the
//!   whole-queue operators (`join/2`, `split/2`) and the deletion family
//!   (`delete/2`, `delete_r/2`). `queue` is pure Erlang over two lists, so every
//!   one of these runs to the oracle's exact value end to end.
//! * **`maps`** — **every operation is a host gap**: the map BIFs
//!   (`erlang:map_get/2`, `maps:get/2`, `maps:find/2`, …) are `erlang` BIFs the
//!   test host's floor does not serve, the NIF-backed exports are
//!   `erlang:nif_error(undef)` stubs, and `maps:to_list/1` reaches the preloaded
//!   `erts_internal:map_next/3`. The module **loads**, but any operation over a map
//!   is a typed [`beam_common::BeamError`] — asserted, never silently skipped, by
//!   [`maps_exports_report_their_host_gap`].
//!
//! Policy: an oracle-agreed target **must** execute here (a typed error where the
//! oracle returns a value FAILS); a target the host cannot serve prints exactly
//! `HOST GAP: <module>:<function>/<arity> — <typed error>` **and asserts the exact
//! typed error**, so a regression turning a typed error into a wrong result — or
//! into a hang — is caught rather than silently passing.

mod support;

use beam_common::BeamError;
use beam_engine::terms;
use beam_term::{make_small, Eterm};
use support::{
    assert_atom, assert_int_list, assert_int_queue, assert_small, assert_term, int_list,
    skip_reason, stdlib_ebin, term_atom, term_list, term_tuple, ModuleHost,
};

// ---------------------------------------------------------------------------
// Host + helpers
// ---------------------------------------------------------------------------

/// A host over the system stdlib `ebin` (there is no fixture harness here).
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    ModuleHost::new(ebin)
}

/// Skips the calling test (loudly) when the oracle stdlib is unavailable.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

/// Asserts `got` is the atom `name`, interning it first so the assertion never
/// depends on some *other* module having happened to mention it.
fn assert_shared_atom(host: &mut ModuleHost, got: Eterm, name: &str) {
    term_atom(host, name);
    assert_atom(host, got, name);
}

/// The `{[Rear], [Front]}` term the oracle prints for a `queue`.
fn queue_term(host: &mut ModuleHost, rear: &[i64], front: &[i64]) -> Eterm {
    let rear = int_list(host, rear);
    let front = int_list(host, front);
    term_tuple(host, &[rear, front])
}

/// Runs a target the host **cannot** serve, prints its `HOST GAP:` line, and
/// returns the typed error. A returned value is a hard failure: these targets are
/// pinned as gaps, so a resolution turning silently into a wrong result must be
/// caught, not printed.
fn host_gap<T>(name: &str, result: Result<T, BeamError>) -> BeamError {
    match result {
        Err(error) => {
            println!("HOST GAP: {name} — {error:?}");
            error
        }
        Ok(_) => panic!("`{name}` is pinned as an unservable host gap, but returned a value"),
    }
}

/// Asserts `err` is the typed BEAM `badarg` — the shape the host's
/// `erlang:nif_error/1` refusal produces for a NIF-backed `maps` export.
fn assert_blocked_badarg(name: &str, err: &BeamError) {
    assert!(matches!(err, BeamError::BadArg), "unexpected `{name}` error: {err:?}");
    assert_eq!(err.as_atom(), Some("badarg"), "`{name}` must be the typed BEAM `badarg`");
}

// ---------------------------------------------------------------------------
// `queue` — the inserters on both ends
// ---------------------------------------------------------------------------

/// The inserters the existing `queue` suites do not cover: `cons/2` and `in_r/2`
/// (both insert at the **front**) and `snoc/2` (the **rear**, the argument-swapped
/// `in/2`) — each verified through the module's internal `{[R],[F]}` shape.
///
/// Opcode families: the `call_ext` cross-module seam into a real `queue.beam`,
/// `test,is_list` guards on both queue halves, `select_val` clause dispatch and
/// the tuple/cons builders behind the returned shape.
#[test]
fn stdlib_queue_front_and_rear_inserts() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:from_list([1,2,3])]),halt().' = {[3,2],[1]}
    let l3 = int_list(&mut host, &[1, 2, 3]);
    let q = host.call("queue", "from_list", &[l3]).expect("queue:from_list/1");
    assert_int_queue(&host, q, &[3, 2], &[1]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:cons(0,queue:from_list([1,2,3]))]),halt().'
    //         = {[3,2],[0,1]}
    let consed = host.call("queue", "cons", &[make_small(0), q]).expect("queue:cons/2");
    assert_int_queue(&host, consed, &[3, 2], &[0, 1]);
    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:to_list(queue:cons(0,queue:from_list([1,2,3])))]),halt().'
    //         = [0,1,2,3]
    let consed_list = host.call("queue", "to_list", &[consed]).expect("queue:to_list/1");
    assert_int_list(&host, consed_list, &[0, 1, 2, 3]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:in_r(0,queue:from_list([1,2,3]))]),halt().'
    //         = {[3,2],[0,1]}  (the same front insert as `cons/2`)
    let in_r = host.call("queue", "in_r", &[make_small(0), q]).expect("queue:in_r/2");
    assert_int_queue(&host, in_r, &[3, 2], &[0, 1]);
    assert_term(&host, in_r, consed);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:snoc(queue:from_list([1,2,3]),0)]),halt().'
    //         = {[0,3,2],[1]}
    let snoc = host.call("queue", "snoc", &[q, make_small(0)]).expect("queue:snoc/2");
    assert_int_queue(&host, snoc, &[0, 3, 2], &[1]);
    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:to_list(queue:snoc(queue:from_list([1,2,3]),0))]),halt().'
    //         = [1,2,3,0]
    let snoc_list = host.call("queue", "to_list", &[snoc]).expect("queue:to_list/1");
    assert_int_list(&host, snoc_list, &[1, 2, 3, 0]);
}

// ---------------------------------------------------------------------------
// `queue` — the rear accessors and removals
// ---------------------------------------------------------------------------

/// The **rear** half of the FIFO: `peek_r/1`, `get_r/1`, `last/1` (a `get_r/1`
/// alias), `out_r/1` (rear removal returning `{What, Queue}`), and the empty-queue
/// `peek_r/1`/`out_r/1` arms.
///
/// Opcode families: the `call_ext` seam, `select_val` clause dispatch over the
/// `{[R],[F]}` shape, the tuple readers/builders, the `lists:last/1` cross-module
/// call and the `test,is_list` guards.
#[test]
fn stdlib_queue_rear_accessors_and_removals() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let l3 = int_list(&mut host, &[1, 2, 3]);
    let q = host.call("queue", "from_list", &[l3]).expect("queue:from_list/1");

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:peek_r(queue:from_list([1,2,3]))]),halt().'
    //         = {value,3}
    let peek_r = host.call("queue", "peek_r", &[q]).expect("queue:peek_r/1");
    let value = term_atom(&mut host, "value");
    let want_peek_r = term_tuple(&mut host, &[value, make_small(3)]);
    assert_term(&host, peek_r, want_peek_r);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:get_r(queue:from_list([1,2,3]))]),halt().' = 3
    let get_r = host.call("queue", "get_r", &[q]).expect("queue:get_r/1");
    assert_small(&host, get_r, 3);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:last(queue:from_list([1,2,3]))]),halt().' = 3
    let last = host.call("queue", "last", &[q]).expect("queue:last/1");
    assert_small(&host, last, 3);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:out_r(queue:from_list([1,2,3]))]),halt().'
    //         = {{value,3},{[2],[1]}}
    let out_r = host.call("queue", "out_r", &[q]).expect("queue:out_r/1");
    let want_head = term_tuple(&mut host, &[value, make_small(3)]);
    let want_rest = queue_term(&mut host, &[2], &[1]);
    let want_out_r = term_tuple(&mut host, &[want_head, want_rest]);
    assert_term(&host, out_r, want_out_r);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:peek_r(queue:new())]),halt().' = empty
    let empty = host.call("queue", "new", &[]).expect("queue:new/0");
    let peek_empty = host.call("queue", "peek_r", &[empty]).expect("queue:peek_r/1 empty");
    assert_shared_atom(&mut host, peek_empty, "empty");

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:out_r(queue:new())]),halt().' = {empty,{[],[]}}
    let out_empty = host.call("queue", "out_r", &[empty]).expect("queue:out_r/1 empty");
    let empty_atom = term_atom(&mut host, "empty");
    let want_out_empty = term_tuple(&mut host, &[empty_atom, empty]);
    assert_term(&host, out_empty, want_out_empty);
}

// ---------------------------------------------------------------------------
// `queue` — joins/splits and the droppers
// ---------------------------------------------------------------------------

/// `join/2` (queue concatenation) and `split/2` (queue ↔ two queues), plus the
/// **rear** droppers `init/1` and its `liat/1` alias, `tail/1` and its `drop/1`
/// alias (front drops).
///
/// Opcode families: `call_ext` into `queue`, the served `lists:reverse/2` NIF
/// inside `join/2`, `lists:split/2` inside `split/2`, and the `++` list operator.
#[test]
fn stdlib_queue_join_split_and_drops() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let l3 = int_list(&mut host, &[1, 2, 3]);
    let q = host.call("queue", "from_list", &[l3]).expect("queue:from_list/1");
    let l45 = int_list(&mut host, &[4, 5]);
    let q45 = host.call("queue", "from_list", &[l45]).expect("queue:from_list/1");

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:join(queue:from_list([1,2,3]),
    //         queue:from_list([4,5]))]),halt().' = {[5],[1,2,3,4]}
    let joined = host.call("queue", "join", &[q, q45]).expect("queue:join/2");
    assert_int_queue(&host, joined, &[5], &[1, 2, 3, 4]);
    // oracle: same join, to_list = [1,2,3,4,5]
    let joined_list = host.call("queue", "to_list", &[joined]).expect("queue:to_list/1");
    assert_int_list(&host, joined_list, &[1, 2, 3, 4, 5]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:split(2,queue:from_list([1,2,3]))]),halt().'
    //         = {{[2],[1]},{[3],[]}}
    let split2 = host.call("queue", "split", &[make_small(2), q]).expect("queue:split/2");
    let left2 = queue_term(&mut host, &[2], &[1]);
    let right2 = queue_term(&mut host, &[3], &[]);
    let want_split2 = term_tuple(&mut host, &[left2, right2]);
    assert_term(&host, split2, want_split2);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:split(0,queue:from_list([1,2,3]))]),halt().'
    //         = {{[],[]},{[3,2],[1]}}  (the degenerate split)
    let split0 = host.call("queue", "split", &[make_small(0), q]).expect("queue:split/2 N=0");
    let left0 = queue_term(&mut host, &[], &[]);
    let want_split0 = term_tuple(&mut host, &[left0, q]);
    assert_term(&host, split0, want_split0);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:init(queue:from_list([1,2,3]))]),halt().'
    //         = {[2],[1]}
    let init = host.call("queue", "init", &[q]).expect("queue:init/1");
    assert_int_queue(&host, init, &[2], &[1]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:liat(queue:from_list([1,2,3]))]),halt().'
    //         = {[2],[1]}  (the `init/1` alias)
    let liat = host.call("queue", "liat", &[q]).expect("queue:liat/1");
    assert_term(&host, liat, init);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:tail(queue:from_list([1,2,3]))]),halt().'
    //         = {[3],[2]}
    let tail = host.call("queue", "tail", &[q]).expect("queue:tail/1");
    assert_int_queue(&host, tail, &[3], &[2]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:drop(queue:from_list([1,2,3]))]),halt().'
    //         = {[3],[2]}  (the `tail/1` alias)
    let drop = host.call("queue", "drop", &[q]).expect("queue:drop/1");
    assert_term(&host, drop, tail);
}

// ---------------------------------------------------------------------------
// `queue` — the deletion family
// ---------------------------------------------------------------------------

/// `delete/2` (first occurrence from the **front**) and `delete_r/2` (first from
/// the **rear**), on a queue with a duplicated element so the two directions
/// differ, plus the not-found arm (which returns the queue unchanged).
///
/// Opcode families: the `call_ext` seam, the delete family's `select_val`
/// dispatch, `test,is_list` guards, the `==` comparison BIF on the match, and the
/// internal `f2r`/`r2f` list shuffles.
#[test]
fn stdlib_queue_delete_from_front_and_rear() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:from_list([1,2,3,2])]),halt().' = {[2,3],[1,2]}
    let l = int_list(&mut host, &[1, 2, 3, 2]);
    let q = host.call("queue", "from_list", &[l]).expect("queue:from_list/1");
    assert_int_queue(&host, q, &[2, 3], &[1, 2]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:delete(2,queue:from_list([1,2,3,2]))]),halt().'
    //         = {[2,3],[1]}
    let deleted = host.call("queue", "delete", &[make_small(2), q]).expect("queue:delete/2");
    assert_int_queue(&host, deleted, &[2, 3], &[1]);
    // oracle: same delete, to_list = [1,3,2]
    let deleted_list = host.call("queue", "to_list", &[deleted]).expect("queue:to_list/1");
    assert_int_list(&host, deleted_list, &[1, 3, 2]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:delete_r(2,queue:from_list([1,2,3,2]))]),halt().'
    //         = {[3],[1,2]}
    let deleted_r = host.call("queue", "delete_r", &[make_small(2), q]).expect("queue:delete_r/2");
    assert_int_queue(&host, deleted_r, &[3], &[1, 2]);
    // oracle: same delete_r, to_list = [1,2,3]
    let deleted_r_list = host.call("queue", "to_list", &[deleted_r]).expect("queue:to_list/1");
    assert_int_list(&host, deleted_r_list, &[1, 2, 3]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[queue:delete(9,queue:from_list([1,2,3,2]))]),halt().'
    //         = {[2,3],[1,2]}  (a missing element leaves the queue untouched)
    let missing = host.call("queue", "delete", &[make_small(9), q]).expect("queue:delete/2 miss");
    assert_term(&host, missing, q);
}

// ---------------------------------------------------------------------------
// `maps` — the whole module is a host gap
// ---------------------------------------------------------------------------

/// The `maps` module **loads** (its literal area holds an external-fun literal
/// `beam_codec` decodes) and `maps:new/0` is pure compiled code, but every
/// *operation* over a map is blocked by the test host's BIF floor: the map BIFs
/// (`erlang:map_get/2`, `maps:get/2`, `maps:find/2`, …) are `erlang` BIFs it does
/// not serve, the NIF-backed exports are `erlang:nif_error(undef)` stubs, and
/// `maps:to_list/1` reaches the preloaded `erts_internal:map_next/3`.
///
/// Each probe is a **real installed `maps` export** over a real test-side map: it
/// prints the required `HOST GAP:` line and asserts the **exact** typed error, so a
/// regression turning a typed error into a fabricated map/list/value — or into a
/// hang — is caught rather than silently passing. Owning crate for closing these
/// gaps: `beam_bifs`'s BIF surface / host-seam adoption, not this engine.
///
/// Opcode families (up to the blocking call): the `call_ext` seam into `maps.beam`,
/// the `get_map_elements`/`put_map` map opcode family over the frozen flat-map
/// layout, `select_val` clause dispatch and the `test,is_map` guard.
#[test]
fn maps_exports_report_their_host_gap() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    host.load("maps").expect("`maps` must load (fun-literal decode)");

    let a = host.intern("a");
    let b = host.intern("b");
    let m_ab = terms::make_flatmap(&mut host, &[(a, make_small(1)), (b, make_small(2))])
        .expect("a test-side #{a => 1, b => 2}");
    let m_a = terms::make_flatmap(&mut host, &[(a, make_small(1))]).expect("test-side #{a => 1}");
    let m_b = terms::make_flatmap(&mut host, &[(b, make_small(2))]).expect("test-side #{b => 2}");
    let keys_a = term_list(&mut host, &[a]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:to_list(#{a=>1,b=>2})]),halt().' = [{a,1},{b,2}]
    // `maps:to_list/1` folds over `erts_internal:map_next/3`; `erts_internal` is a
    // **preloaded** module with no `ebin/erts_internal.beam`, so the cross-module
    // load is the typed `BeamError::Io` (file not found) — never a list.
    let err = host_gap("maps:to_list/1", host.call("maps", "to_list", &[m_ab]));
    assert!(matches!(err, BeamError::Io(_)), "unexpected maps:to_list/1 error: {err:?}");

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:values(#{a=>1})]),halt().' = [1]
    // `maps:values/1` is an `erlang:nif_error(undef)` stub the floor refuses.
    let err = host_gap("maps:values/1", host.call("maps", "values", &[m_a]));
    assert_blocked_badarg("maps:values/1", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:is_key(a,#{a=>1})]),halt().' = true
    let err = host_gap("maps:is_key/2", host.call("maps", "is_key", &[a, m_a]));
    assert_blocked_badarg("maps:is_key/2", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:find(a,#{a=>1})]),halt().' = {ok,1}
    let err = host_gap("maps:find/2", host.call("maps", "find", &[a, m_a]));
    assert_blocked_badarg("maps:find/2", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:merge(#{a=>1},#{b=>2})]),halt().' = #{a => 1,b => 2}
    let err = host_gap("maps:merge/2", host.call("maps", "merge", &[m_a, m_b]));
    assert_blocked_badarg("maps:merge/2", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:with([a],#{a=>1,b=>2})]),halt().' = #{a => 1}
    // `maps:with/2` is `lists:foldl(fun maps:get/2, M, Ks)`; the fold is served and
    // stops at the unserved `maps:get/2`.
    let err = host_gap("maps:with/2", host.call("maps", "with", &[keys_a, m_ab]));
    assert_blocked_badarg("maps:with/2", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:without([a],#{a=>1,b=>2})]),halt().' = #{b => 2}
    // `maps:without/2` is `lists:foldl(fun maps:remove/2, M, Ks)`, stopped by the
    // unserved `maps:remove/2`.
    let err = host_gap("maps:without/2", host.call("maps", "without", &[keys_a, m_ab]));
    assert_blocked_badarg("maps:without/2", &err);
}
