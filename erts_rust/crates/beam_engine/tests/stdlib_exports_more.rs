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

//! **A further real-stdlib exports slice** — oracle-checked end-to-end evidence
//! for exports of `lists`, `string`, `ordsets` and `maps` that the earlier
//! suites do not reach.
//!
//! Every module is loaded straight from the running Erlang's stdlib
//! (`<stdlib>/ebin/<module>.beam`, OTP 29 / `erts-17.1`, the differential oracle)
//! and its exports run on a fresh [`beam_engine::Machine`] over the shared
//! heap/atom host in `support`. Every expected value is the **real** system ERTS
//! result, reproduced with `erl -noshell -eval …` and quoted in an `// oracle:`
//! comment beside its assertion — nothing is guessed — and every test **skips
//! loudly** when the oracle stdlib is unavailable.
//!
//! What this slice adds (all **newly pinned**, none of these exports is covered by
//! `stdlib_lists.rs` / `stdlib_collections*.rs` / `stdlib_text*.rs`):
//!
//! * **`lists`** — the indexed/slicing helpers (`seq/3`, `sublist/3`,
//!   `enumerate/2,3`), the whole zip family (`zip/3`, `zip3/3,4`, `unzip3/1`), the
//!   n-ary/dup-removing merge family (`merge3/3`, `rmerge3/3`, `rmerge/2`,
//!   `umerge3/3`, `rumerge3/3`), the tuple-key family (`keystore/4`, `keymerge/3`,
//!   `ukeysort/2`, `ukeymerge/3`) and `flatten/2`, `uniq/1`, `keysearch/3`.
//! * **`string`** — the obsolete-API char helpers (`len/1`, `concat/2`, `rchr/2`,
//!   `chars/3`, `copies/2`, `words/2`, `sub_word/3`) and the alignment/normaliser
//!   surface (`strip/3`, `left/3`, `right/3`, `centre/3`, `sub_string/3`,
//!   `pad/4`, `take/3`, `equal/3`, `to_lower/1`, `to_upper/1`, `chomp/1`,
//!   `next_codepoint/1`).
//! * **`ordsets`** — `is_equal/2` (the one remaining non-fun export).
//! * **`maps`** — `get/3` (the *default*-carrying lookup, which lowers to
//!   `get_map_elements` and therefore runs where `get/2`'s NIF does not) and
//!   `iterator/1` (the unordered iterator constructor). The remaining `maps`
//!   operations over a map are the **host BIF gaps** [`maps_exports_report_their_host_gap`]
//!   records: the map BIFs the test host's floor does not serve, so a value where
//!   the oracle returns one is a hard failure, never a silently-skipped case.
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
    assert_atom, assert_int_list, assert_small, assert_str, assert_term, chars, int_list,
    skip_reason, stdlib_ebin, term_atom, term_list, term_tuple, ModuleHost,
};

// ---------------------------------------------------------------------------
// Host + helpers
// ---------------------------------------------------------------------------

/// The committed harness fixture (a real `.beam` from the system `erlc`); its
/// wrappers reach the `lists:keysearch/3` NIF and the fun-taking `ordsets`
/// exports a direct call cannot enter.
const HARNESS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/stdlib_exports_harness.beam"
);

/// A host over the system stdlib `ebin` with the harness registered.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    host.register("stdlib_exports_harness", HARNESS_BEAM);
    host
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

/// The Erlang 2-tuple `{A, B}` (the test-side `put_tuple2`).
fn pair(host: &mut ModuleHost, a: Eterm, b: Eterm) -> Eterm {
    term_tuple(host, &[a, b])
}

/// The Erlang 3-tuple `{A, B, C}`.
fn triple(host: &mut ModuleHost, a: Eterm, b: Eterm, c: Eterm) -> Eterm {
    term_tuple(host, &[a, b, c])
}

/// The `{value, Tuple}` result shape `lists:keysearch/3` returns on a hit.
fn value_tuple(host: &mut ModuleHost, tuple: Eterm) -> Eterm {
    let tag = term_atom(host, "value");
    pair(host, tag, tuple)
}

/// The 2-tuple `{A, B}` of two small integers.
fn int_pair(host: &mut ModuleHost, a: i64, b: i64) -> Eterm {
    pair(host, make_small(a), make_small(b))
}

/// The proper list of `{integer, integer}` 2-tuples `rows` (a test-side
/// tuple-key list).
fn int_pair_list(host: &mut ModuleHost, rows: &[(i64, i64)]) -> Eterm {
    let elements: Vec<Eterm> = rows.iter().map(|&(a, b)| int_pair(host, a, b)).collect();
    term_list(host, &elements)
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

/// Asserts `err` is the typed BEAM `badarg` — the shape the host's refusal of an
/// unserved `erlang:*` map BIF produces.
fn blocked_badarg(name: &str, err: &BeamError) {
    assert!(matches!(err, BeamError::BadArg), "unexpected `{name}` error: {err:?}");
    assert_eq!(err.as_atom(), Some("badarg"), "`{name}` must be the typed BEAM `badarg`");
}

// ---------------------------------------------------------------------------
// `lists` — indexed access, slicing and the enumerate family
// ---------------------------------------------------------------------------

/// `lists:seq/3`, `sublist/3` and the `enumerate/2,3` generators — the counting /
/// slicing helpers the reshaping suites do not touch.
///
/// Families: the `gc_bif` arithmetic operators and comparison guards the module
/// uses to build its counters, the cons/tuple builders behind the returned
/// `{Index, Elem}` shapes, and `select_val`/`is_integer` clause dispatch.
#[test]
fn stdlib_exports_lists_indexed_helpers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:seq(1,10,3)]),halt().'
    //         [1,4,7,10]
    let got = host
        .call("lists", "seq", &[make_small(1), make_small(10), make_small(3)])
        .expect("lists:seq/3 must run");
    assert_int_list(&host, got, &[1, 4, 7, 10]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:sublist([1,2,3,4,5],2,3)]),halt().'
    //         [2,3,4]
    let list = int_list(&mut host, &[1, 2, 3, 4, 5]);
    let got = host
        .call("lists", "sublist", &[list, make_small(2), make_small(3)])
        .expect("lists:sublist/3 must run");
    assert_int_list(&host, got, &[2, 3, 4]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:enumerate(5,[a,b])]),halt().'
    //         [{5,a},{6,b}]
    let a = term_atom(&mut host, "a");
    let b = term_atom(&mut host, "b");
    let list = term_list(&mut host, &[a, b]);
    let got = host
        .call("lists", "enumerate", &[make_small(5), list])
        .expect("lists:enumerate/2 must run");
    let first = pair(&mut host, make_small(5), a);
    let second = pair(&mut host, make_small(6), b);
    let want = term_list(&mut host, &[first, second]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:enumerate(0,-2,[a,b])]),halt().'
    //         [{0,a},{-2,b}]
    let a = term_atom(&mut host, "a");
    let b = term_atom(&mut host, "b");
    let list = term_list(&mut host, &[a, b]);
    let got = host
        .call("lists", "enumerate", &[make_small(0), make_small(-2), list])
        .expect("lists:enumerate/3 must run");
    let first = pair(&mut host, make_small(0), a);
    let second = pair(&mut host, make_small(-2), b);
    let want = term_list(&mut host, &[first, second]);
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `lists` — the zip family
// ---------------------------------------------------------------------------

/// `lists:zip/3` (the `{pad, …}` mode), `zip3/3`, `zip3/4` and `unzip3/1`.
///
/// `zip/3`'s third argument is not a default but a *mode* — the atom `trim`
/// (stop at the shortest list), or `{pad, {X, Y}}` (pad the first list with `X`
/// and the second with `Y`) — which is why it exercises the tag/atom dispatch of
/// the module's `select_val` as much as its list builders.
///
/// Families: `select_val` clause dispatch, `test,is_list` guards, the cons and
/// tuple builders, and the cross-module `call_ext` seam.
#[test]
fn stdlib_exports_lists_zip_family() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:zip([1,2,3],[11,22],{pad,{z,undefined}})]),halt().'
    //         [{1,11},{2,22},{3,undefined}]
    let inner = {
        let z = term_atom(&mut host, "z");
        let undefined = term_atom(&mut host, "undefined");
        pair(&mut host, z, undefined)
    };
    let padtag = term_atom(&mut host, "pad");
    let pad = pair(&mut host, padtag, inner);
    let xs = int_list(&mut host, &[1, 2, 3]);
    let ys = int_list(&mut host, &[11, 22]);
    let got = host
        .call("lists", "zip", &[xs, ys, pad])
        .expect("lists:zip/3 must run");
    let undefined = term_atom(&mut host, "undefined");
    let first = pair(&mut host, make_small(1), make_small(11));
    let second = pair(&mut host, make_small(2), make_small(22));
    let third = pair(&mut host, make_small(3), undefined);
    let want = term_list(&mut host, &[first, second, third]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:zip3([1,2],[11,22],[111,222])]),halt().'
    //         [{1,11,111},{2,22,222}]
    let xs = int_list(&mut host, &[1, 2]);
    let ys = int_list(&mut host, &[11, 22]);
    let zs = int_list(&mut host, &[111, 222]);
    let got = host
        .call("lists", "zip3", &[xs, ys, zs])
        .expect("lists:zip3/3 must run");
    let first = triple(&mut host, make_small(1), make_small(11), make_small(111));
    let second = triple(&mut host, make_small(2), make_small(22), make_small(222));
    let want = term_list(&mut host, &[first, second]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:zip3([1,2],[11,22],[111,222],trim)]),halt().'
    //         [{1,11,111},{2,22,222}]
    let trim = term_atom(&mut host, "trim");
    let xs = int_list(&mut host, &[1, 2]);
    let ys = int_list(&mut host, &[11, 22]);
    let zs = int_list(&mut host, &[111, 222]);
    let got = host
        .call("lists", "zip3", &[xs, ys, zs, trim])
        .expect("lists:zip3/4 must run");
    let first = triple(&mut host, make_small(1), make_small(11), make_small(111));
    let second = triple(&mut host, make_small(2), make_small(22), make_small(222));
    let want = term_list(&mut host, &[first, second]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:unzip3([{1,11,111},{2,22,222}])]),halt().'
    //         {[1,2],[11,22],[111,222]}
    let first = triple(&mut host, make_small(1), make_small(11), make_small(111));
    let second = triple(&mut host, make_small(2), make_small(22), make_small(222));
    let rows = term_list(&mut host, &[first, second]);
    let got = host
        .call("lists", "unzip3", &[rows])
        .expect("lists:unzip3/1 must run");
    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[11, 22]);
    let c = int_list(&mut host, &[111, 222]);
    let want = triple(&mut host, a, b, c);
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `lists` — the n-ary / duplicate-removing merge family
// ---------------------------------------------------------------------------

/// `lists:merge3/3`, `rmerge3/3`, `rmerge/2`, `umerge3/3` and `rumerge3/3` — the
/// three-way merges and the reverse merges the earlier suites omit.
///
/// Families: the `call_ext` cross-module seam, comparison guards (`=<`/`<`) as
/// clause dispatch, and the cons builders behind the reverse-merge results.
#[test]
fn stdlib_exports_lists_merge_family() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:merge3([1,4],[2,5],[3,6])]),halt().'
    //         [1,2,3,4,5,6]
    let a = int_list(&mut host, &[1, 4]);
    let b = int_list(&mut host, &[2, 5]);
    let c = int_list(&mut host, &[3, 6]);
    let got = host
        .call("lists", "merge3", &[a, b, c])
        .expect("lists:merge3/3 must run");
    assert_int_list(&host, got, &[1, 2, 3, 4, 5, 6]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:rmerge3([1,4],[2,5],[3,6])]),halt().'
    //         [3,6,2,5,1,4]
    let a = int_list(&mut host, &[1, 4]);
    let b = int_list(&mut host, &[2, 5]);
    let c = int_list(&mut host, &[3, 6]);
    let got = host
        .call("lists", "rmerge3", &[a, b, c])
        .expect("lists:rmerge3/3 must run");
    assert_int_list(&host, got, &[3, 6, 2, 5, 1, 4]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:rmerge([1,3],[2,4])]),halt().'
    //         [2,4,1,3]
    let a = int_list(&mut host, &[1, 3]);
    let b = int_list(&mut host, &[2, 4]);
    let got = host
        .call("lists", "rmerge", &[a, b])
        .expect("lists:rmerge/2 must run");
    assert_int_list(&host, got, &[2, 4, 1, 3]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:umerge3([1,4],[2,5],[3,6])]),halt().'
    //         [1,2,3,4,5,6]
    let a = int_list(&mut host, &[1, 4]);
    let b = int_list(&mut host, &[2, 5]);
    let c = int_list(&mut host, &[3, 6]);
    let got = host
        .call("lists", "umerge3", &[a, b, c])
        .expect("lists:umerge3/3 must run");
    assert_int_list(&host, got, &[1, 2, 3, 4, 5, 6]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:rumerge3([1,4],[2,5],[3,6])]),halt().'
    //         [3,6,2,5,1,4]
    let a = int_list(&mut host, &[1, 4]);
    let b = int_list(&mut host, &[2, 5]);
    let c = int_list(&mut host, &[3, 6]);
    let got = host
        .call("lists", "rumerge3", &[a, b, c])
        .expect("lists:rumerge3/3 must run");
    assert_int_list(&host, got, &[3, 6, 2, 5, 1, 4]);
}

// ---------------------------------------------------------------------------
// `lists` — the tuple-key family
// ---------------------------------------------------------------------------

/// `lists:keystore/4` (both the replace and the append branch),
/// `keymerge/3`, `ukeysort/2` and `ukeymerge/3`.
///
/// Families: tuple indexing (`get_tuple_element`), `select_val` clause dispatch
/// on the key comparison, and the cons/tuple builders. `keystore/4`'s two arms
/// are both pinned (an existing key is replaced, a new key is appended).
#[test]
fn stdlib_exports_lists_key_family() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:keystore(b,1,[{a,1},{b,2}],{b,9})]),halt().'
    //         [{a,1},{b,9}]
    // The **replace** branch: `b` is already a key, so its tuple is overwritten.
    let key = term_atom(&mut host, "b");
    let a_atom = term_atom(&mut host, "a");
    let b_atom = term_atom(&mut host, "b");
    let old = {
        let a = pair(&mut host, a_atom, make_small(1));
        let b = pair(&mut host, b_atom, make_small(2));
        term_list(&mut host, &[a, b])
    };
    let new = pair(&mut host, key, make_small(9));
    let got = host
        .call("lists", "keystore", &[key, make_small(1), old, new])
        .expect("lists:keystore/4 must run");
    let a = pair(&mut host, a_atom, make_small(1));
    let b = pair(&mut host, b_atom, make_small(9));
    let want = term_list(&mut host, &[a, b]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:keystore(c,1,[{1,1},{2,2}],{3,3})]),halt().'
    //         [{1,1},{2,2},{3,3}]
    // The **append** branch: `c` matches no key, so the new tuple is appended.
    let key = term_atom(&mut host, "c");
    let old = int_pair_list(&mut host, &[(1, 1), (2, 2)]);
    let new = int_pair(&mut host, 3, 3);
    let got = host
        .call("lists", "keystore", &[key, make_small(1), old, new])
        .expect("lists:keystore/4 (append) must run");
    let want = int_pair_list(&mut host, &[(1, 1), (2, 2), (3, 3)]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:keymerge(1,[{1,1}],[{3,3}])]),halt().'
    //         [{1,1},{3,3}]
    let a = int_pair_list(&mut host, &[(1, 1)]);
    let b = int_pair_list(&mut host, &[(3, 3)]);
    let got = host
        .call("lists", "keymerge", &[make_small(1), a, b])
        .expect("lists:keymerge/3 must run");
    let want = int_pair_list(&mut host, &[(1, 1), (3, 3)]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:ukeysort(1,[{1,1},{1,2},{2,3}])]),halt().'
    //         [{1,1},{2,3}]   (the first of two equal keys is kept)
    let rows = int_pair_list(&mut host, &[(1, 1), (1, 2), (2, 3)]);
    let got = host
        .call("lists", "ukeysort", &[make_small(1), rows])
        .expect("lists:ukeysort/2 must run");
    let want = int_pair_list(&mut host, &[(1, 1), (2, 3)]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:ukeymerge(1,[{1,1}],[{2,2}])]),halt().'
    //         [{1,1},{2,2}]
    let a = int_pair_list(&mut host, &[(1, 1)]);
    let b = int_pair_list(&mut host, &[(2, 2)]);
    let got = host
        .call("lists", "ukeymerge", &[make_small(1), a, b])
        .expect("lists:ukeymerge/3 must run");
    let want = int_pair_list(&mut host, &[(1, 1), (2, 2)]);
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `lists` — flatten, uniq and keysearch
// ---------------------------------------------------------------------------

/// `lists:flatten/2` (the tail-carrying form) and `uniq/1`.
///
/// Families: the `call_ext` seam, `test,is_list` guards, and the `+`/`-`
/// arithmetic the flatten/uniq counters use.
#[test]
fn stdlib_exports_lists_flatten_and_uniq() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:flatten([1,[2,[3]],[4]],[9])]),halt().'
    //         [1,2,3,4,9]
    let nested = {
        let one = make_small(1);
        let three = make_small(3);
        let inner = term_list(&mut host, &[three]);
        let two = make_small(2);
        let middle = term_list(&mut host, &[two, inner]);
        let four = make_small(4);
        term_list(&mut host, &[one, middle, four])
    };
    let tail = int_list(&mut host, &[9]);
    let got = host
        .call("lists", "flatten", &[nested, tail])
        .expect("lists:flatten/2 must run");
    assert_int_list(&host, got, &[1, 2, 3, 4, 9]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:uniq([1,2,1,3,2])]),halt().'
    //         [1,2,3]
    let list = int_list(&mut host, &[1, 2, 1, 3, 2]);
    let got = host
        .call("lists", "uniq", &[list])
        .expect("lists:uniq/1 must run");
    assert_int_list(&host, got, &[1, 2, 3]);
}

// ---------------------------------------------------------------------------
// `lists` — the `keysearch/3` NIF, and `ordsets` — the fun-taking forms
// ---------------------------------------------------------------------------

/// `lists:keysearch/3` — driven through the harness wrapper, because the
/// installed `lists.beam`'s own `keysearch/3` body is the
/// `erlang:nif_error(undef)` **NIF stub** (verified with `beam_disasm:file/1`), so
/// it can only be entered through a caller's `call_ext` — which the host's BIF
/// floor serves with the same `keyfind` core as `lists:keyfind/3`
/// (`../../erts/emulator/beam/erl_bif_lists.c:1279-1292`).
///
/// Both arms are pinned: the `{value, Tuple}` hit and the `false` miss.
///
/// Families: the `call_ext` seam into `lists.beam`, the NIF dispatch through the
/// host hook, and the tuple builders.
#[test]
fn stdlib_exports_lists_keysearch_via_harness() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:keysearch(b,1,[{a,1},{b,2}])]),halt().'
    //         {value,{b,2}}
    let key = term_atom(&mut host, "b");
    let a_atom = term_atom(&mut host, "a");
    let b_atom = term_atom(&mut host, "b");
    let rows = {
        let a = pair(&mut host, a_atom, make_small(1));
        let b = pair(&mut host, b_atom, make_small(2));
        term_list(&mut host, &[a, b])
    };
    let got = host
        .call("stdlib_exports_harness", "lists_keysearch", &[key, make_small(1), rows])
        .expect("harness lists_keysearch/3 must run");
    let hit = pair(&mut host, b_atom, make_small(2));
    let want = value_tuple(&mut host, hit);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[lists:keysearch(z,1,[{a,1}])]),halt().'
    //         false
    let key = term_atom(&mut host, "z");
    let a_atom = term_atom(&mut host, "a");
    let rows = {
        let a = pair(&mut host, a_atom, make_small(1));
        term_list(&mut host, &[a])
    };
    let got = host
        .call("stdlib_exports_harness", "lists_keysearch", &[key, make_small(1), rows])
        .expect("harness lists_keysearch/3 must run");
    assert_shared_atom(&mut host, got, "false");
}

/// `ordsets:map/2` and `ordsets:filtermap/2` — the last two fun-taking `ordsets`
/// exports, driven through the harness (each needs an anonymous `fun` the caller
/// must build, so it cannot be entered as a direct export).
///
/// Families: `make_fun3` + `call_fun*`, cross-module fun resolution into
/// `ordsets`/`lists`, and the `*`/comparison guards inside the wrappers' funs.
#[test]
fn stdlib_exports_ordsets_fun_forms_via_harness() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[ordsets:map(fun(E) -> E * 2 end, [1,2,3])]),halt().'
    //         [2,4,6]
    let set = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_exports_harness", "ordsets_map_double", &[set])
        .expect("harness ordsets_map_double/1 must run");
    assert_int_list(&host, got, &[2, 4, 6]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[ordsets:filtermap(fun(E) -> {true, E * 2} end, [1,2,3])]),halt().'
    //         [2,4,6]
    let set = int_list(&mut host, &[1, 2, 3]);
    let got = host
        .call("stdlib_exports_harness", "ordsets_filtermap_double", &[set])
        .expect("harness ordsets_filtermap_double/1 must run");
    assert_int_list(&host, got, &[2, 4, 6]);
}

// ---------------------------------------------------------------------------
// `string` — the obsolete-API char helpers
// ---------------------------------------------------------------------------

/// `string:len/1`, `concat/2`, `rchr/2`, `chars/3`, `copies/2`, `words/2` and
/// `sub_word/3` — the obsolete string API that is pure list arithmetic over an
/// `erlang:length/1` / `++` / integer-comparison floor, so it runs end to end.
///
/// Families: the `length` and `++` BIFs, `test,is_list`/`is_integer` guards, and
/// the cons builders behind the char-list results.
#[test]
fn stdlib_exports_string_obsolete_char_helpers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:len("hello")]),halt().'
    //         5
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "len", &[s])
        .expect("string:len/1 must run");
    assert_small(&host, got, 5);

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:concat("ab","cd")]),halt().'
    //         "abcd"
    let s1 = chars(&mut host, "ab");
    let s2 = chars(&mut host, "cd");
    let got = host
        .call("string", "concat", &[s1, s2])
        .expect("string:concat/2 must run");
    assert_str(&host, got, "abcd");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:rchr("hello",$l)]),halt().'
    //         4
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "rchr", &[s, make_small(108)])
        .expect("string:rchr/2 must run");
    assert_small(&host, got, 4);

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:chars($a,3,"bc")]),halt().'
    //         "aaabc"
    let tail = chars(&mut host, "bc");
    let got = host
        .call("string", "chars", &[make_small(97), make_small(3), tail])
        .expect("string:chars/3 must run");
    assert_str(&host, got, "aaabc");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:copies("ab",3)]),halt().'
    //         "ababab"
    let s = chars(&mut host, "ab");
    let got = host
        .call("string", "copies", &[s, make_small(3)])
        .expect("string:copies/2 must run");
    assert_str(&host, got, "ababab");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:words("a,b,c",$\,)]),halt().'
    //         3
    let s = chars(&mut host, "a,b,c");
    let got = host
        .call("string", "words", &[s, make_small(44)])
        .expect("string:words/2 must run");
    assert_small(&host, got, 3);

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:sub_word("a,b,c",2,$\,)]),halt().'
    //         "b"
    let s = chars(&mut host, "a,b,c");
    let got = host
        .call("string", "sub_word", &[s, make_small(2), make_small(44)])
        .expect("string:sub_word/3 must run");
    assert_str(&host, got, "b");
}

// ---------------------------------------------------------------------------
// `string` — padding, alignment and slicing
// ---------------------------------------------------------------------------

/// `string:strip/3`, `left/3`, `right/3`, `centre/3`, `sub_string/3`, `pad/4`
/// and `take/3` — the alignment/normaliser surface.
///
/// Families: `length`/`++`/arithmetic BIFs, `test,is_integer` guards and the
/// deep-list (iolist) builders `string:pad/4` returns.
#[test]
fn stdlib_exports_string_alignment_and_padding() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:strip("xxabxx",both,$x)]),halt().'
    //         "ab"
    let s = chars(&mut host, "xxabxx");
    let both = term_atom(&mut host, "both");
    let got = host
        .call("string", "strip", &[s, both, make_small(120)])
        .expect("string:strip/3 must run");
    assert_str(&host, got, "ab");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:left("abc",6,$.)]),halt().'
    //         "abc..."
    let s = chars(&mut host, "abc");
    let got = host
        .call("string", "left", &[s, make_small(6), make_small(46)])
        .expect("string:left/3 must run");
    assert_str(&host, got, "abc...");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:right("abc",6,$.)]),halt().'
    //         "...abc"
    let s = chars(&mut host, "abc");
    let got = host
        .call("string", "right", &[s, make_small(6), make_small(46)])
        .expect("string:right/3 must run");
    assert_str(&host, got, "...abc");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:centre("abc",7,$.)]),halt().'
    //         "..abc.."
    let s = chars(&mut host, "abc");
    let got = host
        .call("string", "centre", &[s, make_small(7), make_small(46)])
        .expect("string:centre/3 must run");
    assert_str(&host, got, "..abc..");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:sub_string("hello",2,4)]),halt().'
    //         "ell"
    let s = chars(&mut host, "hello");
    let got = host
        .call("string", "sub_string", &[s, make_small(2), make_small(4)])
        .expect("string:sub_string/3 must run");
    assert_str(&host, got, "ell");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:pad("abc",5,leading,$.)]),halt().'
    //         [[46,46],[97,98,99]]   (an iolist: ".." ++ "abc")
    let s = chars(&mut host, "abc");
    let leading = term_atom(&mut host, "leading");
    let got = host
        .call("string", "pad", &[s, make_small(5), leading, make_small(46)])
        .expect("string:pad/4 must run");
    let dots = chars(&mut host, "..");
    let abc = chars(&mut host, "abc");
    let want = term_list(&mut host, &[dots, abc]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:take("abc0z",lists:seq($0,$9),true)]),halt().'
    //         {"abc","0z"}
    let s = chars(&mut host, "abc0z");
    let digits = int_list(&mut host, &[48, 49, 50, 51, 52, 53, 54, 55, 56, 57]);
    let complement = term_atom(&mut host, "true");
    let got = host
        .call("string", "take", &[s, digits, complement])
        .expect("string:take/3 must run");
    let leading = chars(&mut host, "abc");
    let trailing = chars(&mut host, "0z");
    let want = pair(&mut host, leading, trailing);
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `string` — case folding, equality and codepoints
// ---------------------------------------------------------------------------

/// `string:equal/3` (with and without case folding), `to_lower/1`, `to_upper/1`,
/// `chomp/1` and `next_codepoint/1`.
///
/// `chomp/1` is `trim/3` with a multi-cluster separator list, so it exercises
/// the same grapheme scanner `string:trim/3` uses; `next_codepoint/1` reaches
/// `unicode_util:cp/1`, a real cross-module call into the `unicode` support
/// module shipped in the same `ebin`.
///
/// Families: the `call_ext` seam into `unicode_util`/`lists`, `select_val`
/// clause dispatch and the `++`/`length` BIFs.
#[test]
fn stdlib_exports_string_case_and_codepoints() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:equal("abc","abc",true)]),halt().'
    //         true
    let a = chars(&mut host, "abc");
    let b = chars(&mut host, "abc");
    let ignore_case = term_atom(&mut host, "true");
    let got = host
        .call("string", "equal", &[a, b, ignore_case])
        .expect("string:equal/3 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:equal("abc","abd",true)]),halt().'
    //         false
    let a = chars(&mut host, "abc");
    let b = chars(&mut host, "abd");
    let ignore_case = term_atom(&mut host, "true");
    let got = host
        .call("string", "equal", &[a, b, ignore_case])
        .expect("string:equal/3 must run");
    assert_shared_atom(&mut host, got, "false");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:to_lower("AbC")]),halt().'
    //         "abc"
    let s = chars(&mut host, "AbC");
    let got = host
        .call("string", "to_lower", &[s])
        .expect("string:to_lower/1 must run");
    assert_str(&host, got, "abc");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:to_upper("AbC")]),halt().'
    //         "ABC"
    let s = chars(&mut host, "AbC");
    let got = host
        .call("string", "to_upper", &[s])
        .expect("string:to_upper/1 must run");
    assert_str(&host, got, "ABC");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:chomp("abc\n")]),halt().'
    //         "abc"
    let s = chars(&mut host, "abc\n");
    let got = host
        .call("string", "chomp", &[s])
        .expect("string:chomp/1 must run");
    assert_str(&host, got, "abc");

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:next_codepoint("abc")]),halt().'
    //         "abc"
    let s = chars(&mut host, "abc");
    let got = host
        .call("string", "next_codepoint", &[s])
        .expect("string:next_codepoint/1 must run");
    assert_str(&host, got, "abc");
}

// ---------------------------------------------------------------------------
// `ordsets` — is_equal
// ---------------------------------------------------------------------------

/// `ordsets:is_equal/2` — the one remaining non-fun `ordsets` export the earlier
/// suites do not reach (pure list equality over `lists`).
///
/// Families: the `call_ext` seam into `lists`/`ordsets`, `select_val` clause
/// dispatch and the `==`/`=:=` comparison BIFs.
#[test]
fn stdlib_exports_ordsets_is_equal() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[ordsets:is_equal([1,2],[1,2])]),halt().'
    //         true
    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[1, 2]);
    let got = host
        .call("ordsets", "is_equal", &[a, b])
        .expect("ordsets:is_equal/2 must run");
    assert_shared_atom(&mut host, got, "true");

    // oracle: erl -noshell -eval 'io:format("~w~n",[ordsets:is_equal([1],[1,2])]),halt().'
    //         false
    let a = int_list(&mut host, &[1]);
    let b = int_list(&mut host, &[1, 2]);
    let got = host
        .call("ordsets", "is_equal", &[a, b])
        .expect("ordsets:is_equal/2 must run");
    assert_shared_atom(&mut host, got, "false");
}

// ---------------------------------------------------------------------------
// `maps` — the two genuinely-executable exports
// ---------------------------------------------------------------------------

/// `maps:get/3` and `maps:iterator/1` — the only two `maps` exports beyond
/// `new/0` that run on the test host.
///
/// `maps:get/3` is `get(Key, Map, Default)` and compiles to
/// `get_map_elements`/`select_val` with a `Default` fall-through, **not** to the
/// `erlang:map_get/2` NIF that blocks `get/2`, so both its hit and its
/// default arms execute. `maps:iterator/1` is pure Erlang over the map itself
/// (`iterator(M) -> [0 | M]`, `maps.erl:1060-1061`), so it executes too; only
/// `maps:next/1` (the `erts_internal:map_next/3` NIF) blocks.
///
/// Families: the `get_map_elements`/`put_map` map opcode family over the frozen
/// flat-map layout, and the cons builder behind `[0 | Map]`.
#[test]
fn stdlib_exports_maps_get_default_and_iterator() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    host.load("maps").expect("`maps` must load (fun-literal decode)");

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:get(a,#{a=>1},0)]),halt().'
    //         1
    let a = term_atom(&mut host, "a");
    let map = terms::make_flatmap(&mut host, &[(a, make_small(1))]).expect("a test-side #{a => 1}");
    let got = host
        .call("maps", "get", &[a, map, make_small(0)])
        .expect("maps:get/3 must run");
    assert_small(&host, got, 1);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:get(b,#{a=>1},0)]),halt().'
    //         0
    let a = term_atom(&mut host, "a");
    let b = term_atom(&mut host, "b");
    let map = terms::make_flatmap(&mut host, &[(a, make_small(1))]).expect("a test-side #{a => 1}");
    let got = host
        .call("maps", "get", &[b, map, make_small(0)])
        .expect("maps:get/3 (default arm) must run");
    assert_small(&host, got, 0);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:iterator(#{a=>1})]),halt().'
    //         [0|#{a => 1}]
    let a = term_atom(&mut host, "a");
    let map = terms::make_flatmap(&mut host, &[(a, make_small(1))]).expect("a test-side #{a => 1}");
    let got = host
        .call("maps", "iterator", &[map])
        .expect("maps:iterator/1 must run");
    let want = terms::make_cons(&mut host, make_small(0), map).expect("[0 | Map]");
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `maps` — the remaining operations report their host gap
// ---------------------------------------------------------------------------

/// The `maps` exports that are blocked by the test host's BIF floor. The map
/// BIFs (`erlang:map_remove/2`, the `map_update`/`map_take`/`map_intersect`
/// family) are `erlang` BIFs the floor does not serve, and the NIF-backed
/// exports are `erlang:nif_error(undef)` stubs; `maps:from_keys/2` is exactly
/// such a stub (`maps.erl:186`).
///
/// Each probe is a **real installed `maps` export** over a real test-side map: it
/// prints the required `HOST GAP:` line and asserts the **exact** typed error, so
/// a regression turning a typed error into a fabricated map/value — or into a
/// hang — is caught rather than silently passing. Owning crate for closing these
/// gaps: `beam_bifs`'s BIF surface / host-seam adoption, not this engine.
///
/// Families: the `call_ext` seam into `maps.beam` and the `test,is_map` guard.
#[test]
fn maps_exports_report_their_host_gap() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    host.load("maps").expect("`maps` must load (fun-literal decode)");

    let a = term_atom(&mut host, "a");
    let b = term_atom(&mut host, "b");
    let m = terms::make_flatmap(&mut host, &[(a, make_small(1)), (b, make_small(2))])
        .expect("a test-side #{a => 1, b => 2}");
    let keys = term_list(&mut host, &[a, b]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:remove(a,#{a=>1,b=>2})]),halt().' = #{b => 2}
    // `maps:remove/2` is `erlang:map_remove/2`, an unserved map BIF.
    let err = host_gap("maps:remove/2", host.call("maps", "remove", &[a, m]));
    blocked_badarg("maps:remove/2", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:update(a,9,#{a=>1,b=>2})]),halt().' = #{a => 9,b => 2}
    // `maps:update/3` is a `maps` NIF stub.
    let err = host_gap("maps:update/3", host.call("maps", "update", &[a, make_small(9), m]));
    blocked_badarg("maps:update/3", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:take(a,#{a=>1,b=>2})]),halt().' = {1,#{b => 2}}
    // `maps:take/2` is a `maps` NIF stub.
    let err = host_gap("maps:take/2", host.call("maps", "take", &[a, m]));
    blocked_badarg("maps:take/2", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:intersect(#{a=>1,b=>2},#{b=>2})]),halt().' = #{b => 2}
    // `maps:intersect/2` folds over `maps:is_key/2`, itself an unserved map BIF.
    let other = terms::make_flatmap(&mut host, &[(b, make_small(2))]).expect("a test-side #{b => 2}");
    let err = host_gap("maps:intersect/2", host.call("maps", "intersect", &[m, other]));
    blocked_badarg("maps:intersect/2", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[maps:from_keys([a,b],0)]),halt().' = #{a => 0,b => 0}
    // `maps:from_keys/2` is an `erlang:nif_error(undef)` stub (`maps.erl:186`).
    let err = host_gap("maps:from_keys/2", host.call("maps", "from_keys", &[keys, make_small(0)]));
    blocked_badarg("maps:from_keys/2", &err);
}

// ---------------------------------------------------------------------------
// `string` — the preloaded-module gap
// ---------------------------------------------------------------------------

/// `string:list_to_integer/1` reports a **preloaded-module** gap, not a BIF gap:
/// its body is `erts_internal:list_to_integer(String, 10)` (`string.erl:191-193`),
/// and `erts_internal` is **preloaded** — it has no `ebin/erts_internal.beam`, so
/// the cross-module load is the typed [`BeamError::Io`] (file not found), never a
/// fabricated `{Int, Rest}`.
///
/// Owning crate: `beam_rt`'s preloaded-module floor (the runtime that would serve
/// `erts_internal`), not this engine. The probe is a **real installed `string`
/// export**: it prints the required `HOST GAP:` line and asserts the exact typed
/// error, so a regression into a fabricated value is caught, not skipped.
#[test]
fn string_list_to_integer_reports_the_preloaded_module_gap() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // oracle: erl -noshell -eval 'io:format("~w~n",[string:list_to_integer("42abc")]),halt().' = {42,"abc"}
    let s = chars(&mut host, "42abc");
    let err = host_gap("string:list_to_integer/1", host.call("string", "list_to_integer", &[s]));
    assert!(
        matches!(err, BeamError::Io(_)),
        "string:list_to_integer/1 reaches the preloaded `erts_internal`, so it must be the typed Io \
         gap; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Robustness guard (does not depend on the oracle)
// ---------------------------------------------------------------------------

#[test]
fn the_located_stdlib_is_a_usable_stdlib() {
    // Mirrors `tests/stdlib_text_more.rs`: a genuinely independent check of the
    // skip guard's *input* — when the locator claims a stdlib, that directory
    // must actually hold the real modules this suite runs.
    let Some(dir) = stdlib_ebin() else {
        // No locatable stdlib at all: a genuine absent-optional-dependency skip.
        eprintln!("{}", skip_reason().unwrap_or_default());
        return;
    };
    for module in ["lists.beam", "string.beam", "ordsets.beam", "maps.beam"] {
        assert!(
            dir.join(module).is_file(),
            "the located oracle stdlib {dir:?} must hold the real `{module}`"
        );
    }
    assert!(
        skip_reason().is_none(),
        "a located, usable stdlib must not produce a skip reason"
    );
}
