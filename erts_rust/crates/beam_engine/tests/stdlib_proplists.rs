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

//! **`proplists` end-to-end slice** — oracle-checked evidence that the widened
//! typed-IR interpreter runs the real, pure-Erlang `proplists` module from the
//! installed OTP, not a stub.
//!
//! `proplists` (`<stdlib>/ebin/proplists.beam`, OTP 29 / `erts-17.1`, the
//! differential oracle) is loaded straight from the running Erlang's stdlib and
//! its exports are run on a fresh [`beam_engine::Machine`] over the shared
//! heap/atom host in `support`. The module is pure Erlang over cons cells and
//! tuples plus the guard BIFs `is_atom/1`, `is_list/1`, `element/2`,
//! `setelement/3` and `tuple_size/1` — all served by the test host's BIF floor
//! (`tests/support/bifs.rs`) — so nearly every export runs to the oracle's exact
//! value end to end.
//!
//! Newly pinned here (exports the existing suites do **not** cover):
//! `property/1,2`, `unfold/1`, `compact/1`, `get_all_values/2`,
//! `append_values/2`, `substitute_aliases/2`, `substitute_negations/2`,
//! `expand/2`, `normalize/2` and `to_map/1,2`.
//!
//! **The genuinely map-dependent exports are host gaps, not engine gaps.**
//! `get_keys/1` goes through `sets` (`sets:to_list/1` → `maps:keys/1`),
//! `split/2` reaches the unserved `is_map_key/2` / `map_get/2`, `to_map/1,2`'s
//! arity-≥1-but-≠2 clause is `maps:remove/2`, and `from_map/1` is
//! `maps:to_list/1` (the preloaded `erts_internal:map_next/3`). Each is
//! **asserted** as its exact typed [`beam_common::BeamError`] by
//! [`proplists_map_exports_report_their_host_gap`] — never silently skipped, and
//! never a fabricated value. (`to_map/1,2` themselves *do* run: the map literal
//! and `put_map_assoc` are served by the engine's maps family, so they are pinned
//! to their oracle values by [`proplists_to_map`].) Owning crate for closing those
//! gaps: `beam_bifs`'s BIF surface / host-seam adoption, not this engine.
//!
//! Policy: an oracle-agreed target **must** execute here (a typed error where the
//! oracle returns a value FAILS); a target the host cannot serve prints exactly
//! `HOST GAP: <module>:<function>/<arity> — <typed error>` **and asserts the exact
//! typed error**, so a regression turning a typed error into a wrong result — or
//! into a hang — is caught rather than silently passing.
//!
//! Every expected value is the **real** system ERTS result, reproduced with
//! `erl -noshell -eval …` and quoted in an `// oracle:` comment beside its
//! assertion; nothing is guessed. The stdlib is located at **test time**; when
//! `erl`/stdlib is unavailable every test **skips loudly**.

mod support;

use beam_common::BeamError;
use beam_engine::terms;
use beam_term::{make_small, Eterm};
use support::{
    assert_atom, assert_int_list, assert_term, int_list, keyed_tuple, skip_reason,
    stdlib_ebin, term_atom, term_list, term_tuple, ModuleHost,
};

// ---------------------------------------------------------------------------
// Host + helpers
// ---------------------------------------------------------------------------

/// A host over the system stdlib `ebin` (there is no fixture harness here).
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

/// Asserts `got` is the atom `name`, interning it first so the assertion never
/// depends on some *other* module having happened to mention it.
fn assert_shared_atom(host: &mut ModuleHost, got: Eterm, name: &str) {
    term_atom(host, name);
    assert_atom(host, got, name);
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
/// `erlang:nif_error/1` refusal produces for a NIF-backed export.
fn assert_blocked_badarg(name: &str, err: &BeamError) {
    assert!(matches!(err, BeamError::BadArg), "unexpected `{name}` error: {err:?}");
    assert_eq!(err.as_atom(), Some("badarg"), "`{name}` must be the typed BEAM `badarg`");
}

/// Builds the 2-tuple `{Key, Value}` from two already-built terms.
fn pair(host: &mut ModuleHost, key: Eterm, value: Eterm) -> Eterm {
    term_tuple(host, &[key, value])
}

// ---------------------------------------------------------------------------
// `property/1`, `property/2`, `unfold/1`, `compact/1`
// ---------------------------------------------------------------------------

/// `property/1`, `property/2`, `unfold/1` and `compact/1` — the module's
/// representation normalisers.
///
/// Opcode families: the `call_ext` seam into the real `proplists.beam`, the
/// tuple/cons builders behind the returned shapes, the `test,is_atom` guard on
/// `property/1,2`'s head clause, `select_val` clause dispatch and the
/// comprehension (a fresh cons list) behind `compact/1`.
#[test]
fn proplists_property_unfold_and_compact() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let a = term_atom(&mut host, "a");
    let b = term_atom(&mut host, "b");
    let c = term_atom(&mut host, "c");
    let tru = term_atom(&mut host, "true");
    let fal = term_atom(&mut host, "false");
    let one = make_small(1);

    // --- property/1 ---------------------------------------------------------

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:property({a,true})]),halt().' = a
    let arg = pair(&mut host, a, tru);
    let got = host
        .call("proplists", "property", &[arg])
        .expect("proplists:property/1 must run");
    assert_atom(&host, got, "a");

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:property({a,1})]),halt().' = {a,1}
    let arg = pair(&mut host, a, one);
    let got = host
        .call("proplists", "property", &[arg])
        .expect("proplists:property/1 must run");
    assert_term(&host, got, arg);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:property(b)]),halt().' = b
    let got = host
        .call("proplists", "property", &[b])
        .expect("proplists:property/1 must run");
    assert_shared_atom(&mut host, got, "b");

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:property({1,true})]),halt().' = {1,true}
    // The key is not an atom, so the whole term is the normal form.
    let arg = pair(&mut host, one, tru);
    let got = host
        .call("proplists", "property", &[arg])
        .expect("proplists:property/1 must run");
    assert_term(&host, got, arg);

    // --- property/2 ---------------------------------------------------------

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:property(a,true)]),halt().' = a
    let got = host
        .call("proplists", "property", &[a, tru])
        .expect("proplists:property/2 must run");
    assert_atom(&host, got, "a");

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:property(a,1)]),halt().' = {a,1}
    let want = pair(&mut host, a, one);
    let got = host
        .call("proplists", "property", &[a, one])
        .expect("proplists:property/2 must run");
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:property(1,true)]),halt().' = {1,true}
    let want = pair(&mut host, one, tru);
    let got = host
        .call("proplists", "property", &[one, tru])
        .expect("proplists:property/2 must run");
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:property(b,false)]),halt().' = {b,false}
    let want = pair(&mut host, b, fal);
    let got = host
        .call("proplists", "property", &[b, fal])
        .expect("proplists:property/2 must run");
    assert_term(&host, got, want);

    // --- unfold/1 -----------------------------------------------------------

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:unfold([a,{b,1},true,false,c])]),halt().'
    //       = [{a,true},{b,1},{true,true},{false,true},{c,true}]
    let b1 = pair(&mut host, b, one);
    let input = term_list(&mut host, &[a, b1, tru, fal, c]);
    let got = host
        .call("proplists", "unfold", &[input])
        .expect("proplists:unfold/1 must run");
    let e_at = pair(&mut host, a, tru);
    let e_tt = pair(&mut host, tru, tru);
    let e_ft = pair(&mut host, fal, tru);
    let e_ct = pair(&mut host, c, tru);
    let want = term_list(&mut host, &[e_at, b1, e_tt, e_ft, e_ct]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:unfold([])]),halt().' = []
    let empty = term_list(&mut host, &[]);
    let got = host
        .call("proplists", "unfold", &[empty])
        .expect("proplists:unfold/1 must run");
    assert!(terms::list_elements(got).expect("a list").is_empty());

    // --- compact/1 ----------------------------------------------------------

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:compact([{a,true},{b,1},c,{true,true}])]),halt().'
    //       = [a,{b,1},c,true]
    let at = pair(&mut host, a, tru);
    let tt = pair(&mut host, tru, tru);
    let input = term_list(&mut host, &[at, b1, c, tt]);
    let got = host
        .call("proplists", "compact", &[input])
        .expect("proplists:compact/1 must run");
    let want = term_list(&mut host, &[a, b1, c, tru]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:compact([])]),halt().' = []
    let got = host
        .call("proplists", "compact", &[empty])
        .expect("proplists:compact/1 must run");
    assert!(terms::list_elements(got).expect("a list").is_empty());
}

// ---------------------------------------------------------------------------
// `get_all_values/2` and `append_values/2`
// ---------------------------------------------------------------------------

/// `get_all_values/2` (every value for a key) and `append_values/2` (the same,
/// with each value wrapped in a list and the results concatenated).
///
/// Opcode families: `call_ext` into `proplists.beam`, the `tuple_size`/`element`
/// guards, the arity-2 vs arity-3 `case P of {_, Value} -> …` (`select_tuple_arity`
/// already lowered to `TestKind`), the `is_list` guard on `append_values/2`, and
/// the `++` flattened accumulation (`erlang:++/2`).
#[test]
fn proplists_get_all_values_and_append_values() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let a = term_atom(&mut host, "a");
    let b = term_atom(&mut host, "b");
    let c = term_atom(&mut host, "c");
    let z = term_atom(&mut host, "z");
    let tru = term_atom(&mut host, "true");

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:get_all_values(a,[{a,1},{b,2},{a,3},a])]),halt().'
    //       = [1,3,true]
    let a1 = pair(&mut host, a, make_small(1));
    let b2 = pair(&mut host, b, make_small(2));
    let a3 = pair(&mut host, a, make_small(3));
    let input = term_list(&mut host, &[a1, b2, a3, a]);
    let got = host
        .call("proplists", "get_all_values", &[a, input])
        .expect("proplists:get_all_values/2 must run");
    let want = term_list(&mut host, &[make_small(1), make_small(3), tru]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:get_all_values(z,[{a,1}])]),halt().' = []
    let only_a = term_list(&mut host, &[a1]);
    let got = host
        .call("proplists", "get_all_values", &[z, only_a])
        .expect("proplists:get_all_values/2 must run");
    assert!(terms::list_elements(got).expect("a list").is_empty());

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:get_all_values(a,[{a,1,2},{a,3}])]),halt().'
    //       = [3]   (the arity-3 tuple is skipped: `case P of {_, Value}` has no match)
    let a12 = term_tuple(&mut host, &[a, make_small(1), make_small(2)]);
    let input = term_list(&mut host, &[a12, a3]);
    let got = host
        .call("proplists", "get_all_values", &[a, input])
        .expect("proplists:get_all_values/2 must run");
    assert_int_list(&host, got, &[3]);

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:append_values(a,[{a,[1,2]},{b,0},{a,3},{c,-1},{a,[4]}])]),halt().'
    //       = [1,2,3,4]
    let l12 = int_list(&mut host, &[1, 2]);
    let a_l12 = term_tuple(&mut host, &[a, l12]);
    let b_0 = pair(&mut host, b, make_small(0));
    let a_3 = pair(&mut host, a, make_small(3));
    let c_n1 = pair(&mut host, c, make_small(-1));
    let l4 = int_list(&mut host, &[4]);
    let a_l4 = term_tuple(&mut host, &[a, l4]);
    let input = term_list(&mut host, &[a_l12, b_0, a_3, c_n1, a_l4]);
    let got = host
        .call("proplists", "append_values", &[a, input])
        .expect("proplists:append_values/2 must run");
    assert_int_list(&host, got, &[1, 2, 3, 4]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:append_values(a,[{a,[1,2]},a,{a,3}])]),halt().'
    //       = [1,2,true,3]   (the bare atom `a` contributes `true`)
    let input = term_list(&mut host, &[a_l12, a, a_3]);
    let got = host
        .call("proplists", "append_values", &[a, input])
        .expect("proplists:append_values/2 must run");
    let want = term_list(&mut host, &[make_small(1), make_small(2), tru, make_small(3)]);
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `substitute_aliases/2` and `substitute_negations/2`
// ---------------------------------------------------------------------------

/// `substitute_aliases/2` (rename keys) and `substitute_negations/2` (rename and
/// negate a boolean-valued property).
///
/// Opcode families: `call_ext` into `proplists.beam`, the comprehension plus
/// `substitute_*_1` recursion, the `tuple_size`/`element` guards, the
/// `setelement/3` key rewrite (`erlang:setelement/3`), and `select_val` over the
/// `{_, true}` / `{_, false}` / other three-way case in the negation helper.
#[test]
fn proplists_substitute_aliases_and_negations() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let color = term_atom(&mut host, "color");
    let colour = term_atom(&mut host, "colour");
    let other = term_atom(&mut host, "other");
    let red = term_atom(&mut host, "red");
    let blue = term_atom(&mut host, "blue");

    // --- substitute_aliases/2 ----------------------------------------------

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:substitute_aliases([{color,colour}],[color,{color,red},{other,1},{color,blue}])]),halt().'
    //       = [colour,{colour,red},{other,1},{colour,blue}]
    let alias = pair(&mut host, color, colour);
    let aliases = term_list(&mut host, &[alias]);
    let c_red = pair(&mut host, color, red);
    let o_1 = pair(&mut host, other, make_small(1));
    let c_blue = pair(&mut host, color, blue);
    let input = term_list(&mut host, &[color, c_red, o_1, c_blue]);
    let got = host
        .call("proplists", "substitute_aliases", &[aliases, input])
        .expect("proplists:substitute_aliases/2 must run");
    let k_red = pair(&mut host, colour, red);
    let k_blue = pair(&mut host, colour, blue);
    let want = term_list(&mut host, &[colour, k_red, o_1, k_blue]);
    assert_term(&host, got, want);

    // --- substitute_negations/2 --------------------------------------------

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:substitute_negations([{no_foo,foo}],[no_foo,{no_foo,true},{no_foo,false},{no_foo,1},{bar,1}])]),halt().'
    //       = [{foo,false},{foo,false},foo,foo,{bar,1}]
    let no_foo = term_atom(&mut host, "no_foo");
    let foo = term_atom(&mut host, "foo");
    let bar = term_atom(&mut host, "bar");
    let tru = term_atom(&mut host, "true");
    let fal = term_atom(&mut host, "false");
    let neg = pair(&mut host, no_foo, foo);
    let negations = term_list(&mut host, &[neg]);
    let n_true = pair(&mut host, no_foo, tru);
    let n_false = pair(&mut host, no_foo, fal);
    let n_one = pair(&mut host, no_foo, make_small(1));
    let bar_1 = pair(&mut host, bar, make_small(1));
    let input = term_list(&mut host, &[no_foo, n_true, n_false, n_one, bar_1]);
    let got = host
        .call("proplists", "substitute_negations", &[negations, input])
        .expect("proplists:substitute_negations/2 must run");
    let f_false = pair(&mut host, foo, fal);
    let want = term_list(&mut host, &[f_false, f_false, foo, foo, bar_1]);
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `expand/2`
// ---------------------------------------------------------------------------

/// `expand/2` — expand a matching property into a list of terms and drop any
/// later entries sharing its key.
///
/// Opcode families: `call_ext` into `proplists.beam`, the tuple-comprehension
/// normalisation of the expansions, `key_uniq` (tuple heads + `element`/`=:=`),
/// the `expand_0..3` recursion, `flatten/1` (whose `is_list` guard picks the `++`
/// arm) and the `delete/2` call on the expanded key.
#[test]
fn proplists_expand() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let foo = term_atom(&mut host, "foo");
    let fie = term_atom(&mut host, "fie");
    let fum = term_atom(&mut host, "fum");
    let bar = term_atom(&mut host, "bar");
    let baz = term_atom(&mut host, "baz");
    let fal = term_atom(&mut host, "false");
    let tru = term_atom(&mut host, "true");

    let expansion = term_list(&mut host, &[bar, baz]);

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:expand([{foo,[bar,baz]}],[fie,foo,fum])]),halt().' = [fie,bar,baz,fum]
    let e_foo = pair(&mut host, foo, expansion);
    let expansions = term_list(&mut host, &[e_foo]);
    let input = term_list(&mut host, &[fie, foo, fum]);
    let got = host
        .call("proplists", "expand", &[expansions, input])
        .expect("proplists:expand/2 must run");
    let want = term_list(&mut host, &[fie, bar, baz, fum]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:expand([{{foo,false},[bar,baz]}],[fie,{foo,false},fum])]),halt().'
    //       = [fie,bar,baz,fum]
    let foo_false = pair(&mut host, foo, fal);
    let e_ff = pair(&mut host, foo_false, expansion);
    let expansions = term_list(&mut host, &[e_ff]);
    let input = term_list(&mut host, &[fie, foo_false, fum]);
    let got = host
        .call("proplists", "expand", &[expansions, input])
        .expect("proplists:expand/2 must run");
    let want = term_list(&mut host, &[fie, bar, baz, fum]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:expand([{{foo,true},[bar,baz]}],[{foo,false},fie,foo,fum])]),halt().'
    //       = [{foo,false},fie,foo,fum]   ({foo,false} shadows the `foo` expansion)
    let foo_true = pair(&mut host, foo, tru);
    let e_ft = pair(&mut host, foo_true, expansion);
    let expansions = term_list(&mut host, &[e_ft]);
    let input = term_list(&mut host, &[foo_false, fie, foo, fum]);
    let got = host
        .call("proplists", "expand", &[expansions, input])
        .expect("proplists:expand/2 must run");
    let want = term_list(&mut host, &[foo_false, fie, foo, fum]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:expand([{foo,[bar,baz]}],[foo,foo,fum])]),halt().' = [bar,baz,fum]
    // The duplicate `foo` after the expansion is dropped by `delete/2`.
    let expansions = term_list(&mut host, &[e_foo]);
    let input = term_list(&mut host, &[foo, foo, fum]);
    let got = host
        .call("proplists", "expand", &[expansions, input])
        .expect("proplists:expand/2 must run");
    let want = term_list(&mut host, &[bar, baz, fum]);
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `normalize/2`
// ---------------------------------------------------------------------------

/// `normalize/2` — run a list of substitution/expansion stages and compact the
/// result. Each stage list here carries exactly one operation.
///
/// Opcode families: `call_ext` into `proplists.beam`, `apply_stages/2`'s
/// `select_val` over the `aliases`/`expand`/`negations` operation tags, and the
/// `apply_stages → substitute_* → expand → compact` chain (`erlang:++/2`,
/// `setelement/3`, `delete/2` internal calls).
#[test]
fn proplists_normalize() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let foo = term_atom(&mut host, "foo");
    let no_foo = term_atom(&mut host, "no_foo");
    let color = term_atom(&mut host, "color");
    let colour = term_atom(&mut host, "colour");
    let red = term_atom(&mut host, "red");
    let fie = term_atom(&mut host, "fie");
    let fum = term_atom(&mut host, "fum");
    let bar = term_atom(&mut host, "bar");
    let baz = term_atom(&mut host, "baz");
    let tru = term_atom(&mut host, "true");
    let fal = term_atom(&mut host, "false");
    let a = term_atom(&mut host, "a");
    let b = term_atom(&mut host, "b");
    let negations_tag = term_atom(&mut host, "negations");
    let aliases_tag = term_atom(&mut host, "aliases");
    let expand_tag = term_atom(&mut host, "expand");

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:normalize([{no_foo,true},foo],[{negations,[{no_foo,foo}]}])]),halt().'
    //       = [{foo,false},foo]
    let neg = pair(&mut host, no_foo, foo);
    let negs = term_list(&mut host, &[neg]);
    let stage = pair(&mut host, negations_tag, negs);
    let stages = term_list(&mut host, &[stage]);
    let n_true = pair(&mut host, no_foo, tru);
    let input = term_list(&mut host, &[n_true, foo]);
    let got = host
        .call("proplists", "normalize", &[input, stages])
        .expect("proplists:normalize/2 must run");
    let foo_false = pair(&mut host, foo, fal);
    let want = term_list(&mut host, &[foo_false, foo]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:normalize([color,{color,red}],[{aliases,[{color,colour}]}])]),halt().'
    //       = [colour,{colour,red}]
    let alias = pair(&mut host, color, colour);
    let aliases = term_list(&mut host, &[alias]);
    let stage = pair(&mut host, aliases_tag, aliases);
    let stages = term_list(&mut host, &[stage]);
    let c_red = pair(&mut host, color, red);
    let input = term_list(&mut host, &[color, c_red]);
    let got = host
        .call("proplists", "normalize", &[input, stages])
        .expect("proplists:normalize/2 must run");
    let k_red = pair(&mut host, colour, red);
    let want = term_list(&mut host, &[colour, k_red]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval
    //   'io:format("~w~n",[proplists:normalize([fie,foo,fum],[{expand,[{foo,[bar,baz]}]}])]),halt().'
    //       = [fie,bar,baz,fum]
    let expansion = term_list(&mut host, &[bar, baz]);
    let e_foo = pair(&mut host, foo, expansion);
    let expansions = term_list(&mut host, &[e_foo]);
    let stage = pair(&mut host, expand_tag, expansions);
    let stages = term_list(&mut host, &[stage]);
    let input = term_list(&mut host, &[fie, foo, fum]);
    let got = host
        .call("proplists", "normalize", &[input, stages])
        .expect("proplists:normalize/2 must run");
    let want = term_list(&mut host, &[fie, bar, baz, fum]);
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:normalize([{a,true},{b,true}],[])]),halt().'
    //       = [a,b]   (no stages ⇒ just `compact/1`)
    let a_true = pair(&mut host, a, tru);
    let b_true = pair(&mut host, b, tru);
    let input = term_list(&mut host, &[a_true, b_true]);
    let no_stages = term_list(&mut host, &[]);
    let got = host
        .call("proplists", "normalize", &[input, no_stages])
        .expect("proplists:normalize/2 must run");
    let want = term_list(&mut host, &[a, b]);
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// `to_map/1`, `to_map/2`
// ---------------------------------------------------------------------------

/// `to_map/1` and `to_map/2` — a proplist converted to a map (the map literal /
/// `put_map_assoc` path is served by the engine's maps family), and the same
/// through an empty stage list.
///
/// Note the *degradation* pinned separately below: an input carrying a tuple of
/// arity ≥ 3-but-≠-2 sends the `foldr` fun down its `maps:remove/2` clause, which
/// the host's BIF floor does not serve.
///
/// Opcode families: `call_ext` into `proplists.beam`, the `foldr` fun through
/// the `call_fun` seam, the fun's `select_tuple_arity`/`test,is_atom` clause
/// dispatch, the `put_map_assoc` map builder over the frozen flat-map layout and
/// the empty-stage-list `apply_stages` no-op.
#[test]
fn proplists_to_map() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let a = term_atom(&mut host, "a");
    let b = term_atom(&mut host, "b");
    let tru = term_atom(&mut host, "true");

    let b1 = keyed_tuple(&mut host, "b", 1);
    let props = term_list(&mut host, &[a, b1]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:to_map([a,{b,1}])]),halt().'
    //       = #{b => 1,a => true}
    let got = host
        .call("proplists", "to_map", &[props])
        .expect("proplists:to_map/1 must run");
    let want = terms::make_flatmap(&mut host, &[(a, tru), (b, make_small(1))])
        .expect("a test-side #{a => true, b => 1}");
    assert_term(&host, got, want);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:to_map([a,{b,1}],[])]),halt().'
    //       = #{b => 1,a => true}
    let no_stages = term_list(&mut host, &[]);
    let got = host
        .call("proplists", "to_map", &[props, no_stages])
        .expect("proplists:to_map/2 must run");
    let want = terms::make_flatmap(&mut host, &[(a, tru), (b, make_small(1))])
        .expect("a test-side #{a => true, b => 1}");
    assert_term(&host, got, want);
}

// ---------------------------------------------------------------------------
// The map-dependent exports — the host gaps
// ---------------------------------------------------------------------------

/// The `proplists` exports that are blocked by the test host's BIF floor because
/// they rest on map operations it does not serve.
///
/// * `get_keys/1` funnels through `sets` (`sets:to_list/1` → `maps:keys/1`, an
///   `erlang:nif_error(undef)` stub);
/// * `split/2` builds a `#{K => []}` store and then probes it with
///   `is_map_key/2` / reads it with `map_get/2`, neither served;
/// * `from_map/1` is `maps:to_list/1`, which folds over the **preloaded**
///   `erts_internal:map_next/3` — a module with no `ebin/erts_internal.beam`, so
///   the cross-module load is the typed [`BeamError::Io`] (file not found);
/// * `to_map/1`'s arity-≥1-but-≠2 clause is `maps:remove/2`, unserved — so an
///   input with such a tuple is a `badarg` even though `to_map/1` itself runs
///   (pinned to its oracle value by [`proplists_to_map`]); `to_map/2` inherits it
///   by delegation.
///
/// Each probe is a **real installed `proplists` export** over a real test-side
/// proplist: it prints the required `HOST GAP:` line and asserts the **exact**
/// typed error, so a regression turning a typed error into a fabricated
/// map/list/value — or into a hang — is caught rather than silently passing.
/// Owning crate for closing these gaps: `beam_bifs`'s BIF surface / host-seam
/// adoption, not this engine.
#[test]
fn proplists_map_exports_report_their_host_gap() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    host.load("proplists").expect("`proplists` must load");

    let a = term_atom(&mut host, "a");
    let b1 = keyed_tuple(&mut host, "b", 1);
    let props = term_list(&mut host, &[a, b1]);
    let keys = term_list(&mut host, &[a]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:get_keys([a,{b,1}])]),halt().' = [a,b]
    // `sets:to_list/1` on a v2 set (a map) calls `maps:keys/1` — the host's
    // `erlang:nif_error(undef)` refusal.
    let err = host_gap("proplists:get_keys/1", host.call("proplists", "get_keys", &[props]));
    assert_blocked_badarg("proplists:get_keys/1", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:split([a,{b,1}],[a])]),halt().'
    //       = {[[a]],[{b,1}]}
    // `split/2` probes its `#{K => []}` store with `is_map_key/2`, unserved.
    let err = host_gap("proplists:split/2", host.call("proplists", "split", &[props, keys]));
    assert_blocked_badarg("proplists:split/2", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:to_map([a,{a,1,2}])]),halt().'
    //       = #{a => true}   (the arity-3 tuple takes the `maps:remove/2` clause)
    // `maps:remove/2` is an `erlang:nif_error(undef)` stub the floor refuses, so
    // this *shape* of `to_map/1` is blocked even though the ordinary one runs.
    let a12 = term_tuple(&mut host, &[a, make_small(1), make_small(2)]);
    let props3 = term_list(&mut host, &[a, a12]);
    let err = host_gap(
        "proplists:to_map/1 (arity-3 tuple)",
        host.call("proplists", "to_map", &[props3]),
    );
    assert_blocked_badarg("proplists:to_map/1 (arity-3 tuple)", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:to_map([a,{a,1,2}],[])]),halt().'
    //       = #{a => true}
    // `to_map/2` delegates to `to_map/1` (`apply_stages/2` with `[]` is the
    // identity), so it inherits the same `maps:remove/2` gap for this shape.
    let no_stages = term_list(&mut host, &[]);
    let err = host_gap(
        "proplists:to_map/2 (arity-3 tuple)",
        host.call("proplists", "to_map", &[props3, no_stages]),
    );
    assert_blocked_badarg("proplists:to_map/2 (arity-3 tuple)", &err);

    // oracle: erl -noshell -eval 'io:format("~w~n",[proplists:from_map(#{a=>1})]),halt().' = [{a,1}]
    // `maps:to_list/1` folds over the preloaded `erts_internal:map_next/3`; there
    // is no `ebin/erts_internal.beam`, so the cross-module load is the typed
    // `BeamError::Io` — never a list.
    let m_a = terms::make_flatmap(&mut host, &[(a, make_small(1))]).expect("a test-side #{a => 1}");
    let err = host_gap("proplists:from_map/1", host.call("proplists", "from_map", &[m_a]));
    assert!(matches!(err, BeamError::Io(_)), "unexpected proplists:from_map/1 error: {err:?}");
}

// ---------------------------------------------------------------------------
// Robustness guard (does not depend on the oracle)
// ---------------------------------------------------------------------------

#[test]
fn the_located_stdlib_is_a_usable_stdlib() {
    // A genuinely independent check of the skip guard's *input* — when the
    // locator claims a stdlib, that directory must actually hold the real
    // `proplists.beam`.
    let Some(dir) = stdlib_ebin() else {
        eprintln!("{}", skip_reason().unwrap_or_default());
        return;
    };
    assert!(
        dir.join("proplists.beam").is_file(),
        "the located oracle stdlib {dir:?} must hold the real `proplists.beam`"
    );
    assert!(
        skip_reason().is_none(),
        "a located, usable stdlib must not produce a skip reason"
    );
}

