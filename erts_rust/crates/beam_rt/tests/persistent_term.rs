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

//! The seven `persistent_term:*` BIFs, end to end through the runtime glue.
//!
//! `beam_bifs`'s `persistent_term.rs` registers and renders all seven BIFs but cannot hold
//! the store itself: persistent terms are a **node-global** key/value table
//! (`the_hash_table`, `../../erts/emulator/beam/erl_bif_persistent.c:187`) outside every
//! process heap, updated under a table-wide lock with thread-progress hand-off — Stage-5
//! runtime state. Every operation therefore goes through the optional `PersistentTerm` host
//! seam, and `beam_rt::context::call_registered` installs the concrete `PersistentTermBridge`
//! over `Runtime`'s shared `Rc<RefCell<PersistentTermStore>>` (the port/NIF/ETS pattern).
//! This suite pins that whole path at the library boundary — the BIF's argument handling, the
//! `ok`/`true`/`false`/`badarg` outcomes, and the `get/0` list and `info/0` map shapes.
//!
//! # Oracle ground truth (OTP 29.1.1 / erts-17.1)
//!
//! ```text
//! $ erl -noshell -eval '
//!     P=persistent_term:put(mi_k,1), G=persistent_term:get(mi_k),
//!     D=persistent_term:get(mi_missing,default_v),
//!     E1=persistent_term:erase(mi_k), E2=persistent_term:erase(mi_k),
//!     PN1=persistent_term:put_new(mi_n,1), PN2=persistent_term:put_new(mi_n,1),
//!     io:format("~p ~p ~p ~p ~p ~p ~p ~p~n",[G,D,E1,E2,PN1,PN2,
//!         is_badarg(persistent_term:put_new(mi_n,2)),
//!         is_badarg(persistent_term:get(mi_missing))]),
//!     io:format("info=~p~n",[persistent_term:info()]), halt(0).' -s init stop
//! 1 default_v true false ok ok true true
//! info=#{count => 27,memory => 33352}
//! ```
//!
//! so `put/2` is `ok`, `erase/1` is **`true`/`false`** (not `ok`), `get/1` of an absent key
//! is `badarg`, `put_new/2` of a key holding a **different** value is `badarg` while the
//! same value is a no-op `ok`, and `info/0` is the flat map `#{count => N, memory => Bytes}`.
//! The oracle's `count`/`memory` include its own ~27 predefined persistent terms and its
//! literal-area overhead; this runtime's are its own bookkeeping (the entry count and the
//! heap-word total of the stored terms), so only the **shape** is compared in
//! [`the_goldens_match_the_live_oracle`].
//!
//! The suite needs no toolchain for its assertions; only the oracle comparison needs `erl`,
//! and it skips **loudly** without it.
mod support;

use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_small, make_small, signed_value, Eterm};

use support::assert_badarg;

/// A `Runtime` whose node-global persistent-term store starts empty.
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// Run `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are
/// dropped before `rt` is borrowed again.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

/// Call a `persistent_term:*` BIF through the runtime's `ExecutionContext` seam.
fn call(rt: &mut Runtime, name: &str, arity: u32, args: &[Eterm]) -> beam_common::Result<Eterm> {
    with_context(rt, |ctx| ctx.call_bif("persistent_term", name, arity, args))
}

/// An atom as the runtime names it, for the scalar comparison helpers below.
fn scalar(rt: &Runtime, term: Eterm) -> String {
    if is_small(term) {
        signed_value(term).to_string()
    } else {
        rt.atom_name_of(term)
            .unwrap_or_else(|| panic!("{term:?} is neither a small integer nor a named atom"))
            .to_string()
    }
}

// ---------------------------------------------------------------------------------------
// The seven BIFs
// ---------------------------------------------------------------------------------------

/// The complete golden sequence: `put/2` ⇒ `ok`, `get/1` ⇒ the value, `get/2` ⇒ the default,
/// `get/1` of an absent key ⇒ `badarg`, `put_new/2` onto a *different* value ⇒ `badarg` and
/// onto the *same* value ⇒ `ok`, `erase/1` ⇒ `true` then `false`, `get/0` ⇒ the stored
/// `{Key, Value}` pairs, `info/0` ⇒ `#{count => N, memory => Bytes}`.
#[test]
fn the_seven_bifs_follow_the_oracle() {
    let mut rt = runtime();
    let k = rt.intern("k");
    let missing = rt.intern("missing_key_xyz");
    let default = rt.intern("default_value_xyz");
    let ok_atom = rt.intern("ok");
    let true_term = rt.intern("true");
    let false_term = rt.intern("false");
    let one = make_small(1);
    let two = make_small(2);

    assert_eq!(
        call(&mut rt, "put", 2, &[k, one]).expect("put(k, 1)"),
        ok_atom,
        "put/2 returns ok"
    );
    assert_eq!(
        call(&mut rt, "get", 1, &[k]).expect("get(k)"),
        one,
        "get/1 returns the stored value"
    );
    assert_eq!(
        call(&mut rt, "get", 2, &[missing, default]).expect("get(missing, Default)"),
        default,
        "get/2 returns the default for an absent key"
    );
    assert_badarg(
        call(&mut rt, "get", 1, &[missing]).expect_err("get(missing) is badarg"),
    );
    assert_badarg(
        call(&mut rt, "put_new", 2, &[k, two]).expect_err("put_new onto a different value"),
    );
    assert_eq!(
        call(&mut rt, "put_new", 2, &[k, one]).expect("put_new onto the same value"),
        ok_atom,
        "put_new/2 of a key already holding that exact value is a no-op ok"
    );

    // `get/0` — the stored `{Key, Value}` pairs, in the store's (insertion) order. The
    // oracle's own order is an unspecified hash-table walk, so only this runtime's order
    // is pinned (insertion order, stable across an overwrite).
    let all = call(&mut rt, "get", 0, &[]).expect("get()");
    let pairs = terms::list_elements(all).expect("get/0 is a proper list");
    assert_eq!(pairs.len(), 1, "exactly the one stored entry: {pairs:?}");
    assert_eq!(terms::tuple_arity(pairs[0]).expect("a 2-tuple"), 2);
    assert_eq!(terms::tuple_element(pairs[0], 0).expect("the key"), k);
    assert_eq!(terms::tuple_element(pairs[0], 1).expect("the value"), one);

    // `info/0` — a **flat map** `#{count => N, memory => Bytes}` with this runtime's real
    // bookkeeping: one entry, and the heap-word total of the stored terms in bytes.
    let info = call(&mut rt, "info", 0, &[]).expect("info()");
    let entries = terms::map_entries(info).expect("info/0 is a map");
    assert_eq!(entries.len(), 2, "the `count` and `memory` keys: {entries:?}");
    let count = map_value(&rt, info, "count").expect("an info/0 `count` key");
    let memory = map_value(&rt, info, "memory").expect("an info/0 `memory` key");
    assert!(is_small(count), "count is an integer: {count:?}");
    assert_eq!(signed_value(count), 1, "one stored entry");
    assert!(is_small(memory), "memory is an integer: {memory:?}");
    assert!(signed_value(memory) >= 0, "memory is non-negative");

    // `erase/1` ⇒ `true` for a present key, then `false` (`erl_bif_persistent.c:539,548`).
    assert_eq!(
        call(&mut rt, "erase", 1, &[k]).expect("erase(k)"),
        true_term,
        "erase/1 of a present key is true"
    );
    assert_eq!(
        call(&mut rt, "erase", 1, &[k]).expect("erase(k) again"),
        false_term,
        "erase/1 of an absent key is false"
    );

    // The store is empty again: `get/0` ⇒ `[]` and `info/0`'s `count` ⇒ 0.
    assert_eq!(
        terms::list_elements(call(&mut rt, "get", 0, &[]).expect("get()")).expect("a list"),
        vec![],
        "the store is empty"
    );
    let empty_info = call(&mut rt, "info", 0, &[]).expect("info()");
    let count = map_value(&rt, empty_info, "count").expect("a `count` key");
    assert_eq!(signed_value(count), 0, "no entries left");
}

/// `put/2` of an existing key **overwrites in place** (never reorders), and a second,
/// distinct key appends — so `get/0` keeps insertion order.
#[test]
fn put_overwrites_in_place_and_get_all_keeps_insertion_order() {
    let mut rt = runtime();
    let k = rt.intern("k");
    let n = rt.intern("n");
    let ok_atom = rt.intern("ok");
    let one = make_small(1);
    let nine = make_small(9);

    assert_eq!(
        call(&mut rt, "put", 2, &[k, one]).expect("put(k, 1)"),
        ok_atom
    );
    assert_eq!(
        call(&mut rt, "put", 2, &[k, nine]).expect("put(k, 9)"),
        ok_atom,
        "an overwrite is ok"
    );
    assert_eq!(
        call(&mut rt, "get", 1, &[k]).expect("get(k)"),
        nine,
        "the overwrite took effect"
    );

    call(&mut rt, "put", 2, &[n, one]).expect("put(n, 1)");
    let all = call(&mut rt, "get", 0, &[]).expect("get()");
    let pairs = terms::list_elements(all).expect("a proper list");
    let keys: Vec<Eterm> = pairs
        .iter()
        .map(|pair| terms::tuple_element(*pair, 0).expect("the key"))
        .collect();
    assert_eq!(keys, vec![k, n], "insertion order survives an overwrite");
}

/// Any term may be a key: an atom, an integer, a binary, a tuple and a list all store and
/// read back (the oracle accepts every term shape — `beam_bifs/src/persistent_term.rs`).
///
/// The boxed keys come from `Runtime::eval`, whose results are built on the runtime's own
/// (never-reset) process heap — the same pointer stability the store's raw-`Eterm` design
/// relies on.
#[test]
fn any_term_can_be_a_key() {
    let mut rt = runtime();
    let atom = rt.intern("an_atom_key");
    let binary = rt.eval("<<1,2,3>>").expect("a binary literal");
    let tuple = rt.eval("{a,2}").expect("a tuple literal");
    let list = rt.eval("[1,2,3]").expect("a list literal");
    let value = rt.intern("stored");
    let ok_atom = rt.intern("ok");

    for key in [atom, make_small(7), binary, tuple, list] {
        assert_eq!(
            call(&mut rt, "put", 2, &[key, value]).expect("put(Key, stored)"),
            ok_atom
        );
        assert_eq!(
            call(&mut rt, "get", 1, &[key]).expect("get(Key)"),
            value,
            "the value read back for key {key:?}"
        );
    }
    let all = call(&mut rt, "get", 0, &[]).expect("get()");
    assert_eq!(
        terms::list_elements(all).expect("a list").len(),
        5,
        "five distinct keys"
    );
}

// ---------------------------------------------------------------------------------------
// Boxed keys: structural, not word, identity
// ---------------------------------------------------------------------------------------

/// A **boxed** key must resolve through a structurally equal term built *independently*.
///
/// The oracle's `lookup` compares a bucket with `EQ(key, …)` — the exact (`=:=`) term
/// comparison (`erl_bif_persistent.c:1015`) — and `put_common`'s value no-op check uses the
/// same `EQ` (`:726`). Word identity is *not* enough: two equal boxed terms are two
/// distinct heap words, so a raw-word comparison makes `get/1` report the oracle's `badarg`
/// and `erase/1` its `false`.
///
/// This is the regression the immediate-key tests missed: `rt.eval` of the same literal
/// twice yields structurally equal but **distinct** heap words (asserted below), whereas
/// equal atoms/integers are the *same* immediate word.
#[test]
fn boxed_keys_match_an_equal_but_distinct_term() {
    let mut rt = runtime();
    let ok_atom = rt.intern("ok");
    let true_term = rt.intern("true");
    let value = rt.intern("stored");

    for literal in ["{k,1}", "[1,2,3]", "#{a => 1}", "<<1,2,3>>"] {
        let key = rt
            .eval(literal)
            .unwrap_or_else(|e| panic!("{literal}: {e:?}"));
        let equal = rt
            .eval(literal)
            .unwrap_or_else(|e| panic!("{literal} again: {e:?}"));
        assert_ne!(
            key, equal,
            "`{literal}` evaluated twice must be two distinct heap words \
             (otherwise this test proves nothing)"
        );

        assert_eq!(
            call(&mut rt, "put", 2, &[key, value]).expect("put(Key, Value)"),
            ok_atom,
            "put/2 of a fresh {literal} key"
        );
        assert_eq!(
            call(&mut rt, "get", 1, &[equal]).expect("get(Equal)"),
            value,
            "get/1 through a structurally equal {literal} key"
        );
        let all = call(&mut rt, "get", 0, &[]).expect("get()");
        assert_eq!(
            terms::list_elements(all).expect("a proper list").len(),
            1,
            "the equal {literal} key must not append a second entry"
        );
        assert_eq!(
            call(&mut rt, "erase", 1, &[equal]).expect("erase(Equal)"),
            true_term,
            "erase/1 through a structurally equal {literal} key"
        );
        assert_badarg(call(&mut rt, "get", 1, &[equal]).expect_err("the erased entry is gone"));
    }
}

/// `put_new/2`'s no-op `ok` compares the stored **value** structurally too: a structurally
/// equal but distinct value is `ok`, a different one the oracle's `badarg`
/// (`put_common:726-733`).
#[test]
fn put_new_matches_a_boxed_value_structurally() {
    let mut rt = runtime();
    let ok_atom = rt.intern("ok");
    let key = rt.eval("{the_key}").expect("a boxed key literal");
    let value = rt.eval("{1,2}").expect("a boxed value literal");
    // The same literals, built again: equal-but-distinct heap words.
    let equal_key = rt.eval("{the_key}").expect("the key again");
    let equal_value = rt.eval("{1,2}").expect("the value again");
    let other = rt.eval("{1,3}").expect("a different value");
    assert_ne!(value, equal_value, "the second value is a distinct word");

    assert_eq!(
        call(&mut rt, "put", 2, &[key, value]).expect("put(Key, Value)"),
        ok_atom
    );
    assert_eq!(
        call(&mut rt, "put_new", 2, &[equal_key, equal_value])
            .expect("put_new onto a structurally equal value"),
        ok_atom,
        "an equal-but-distinct value is the no-op ok, not badarg"
    );
    assert_badarg(call(&mut rt, "put_new", 2, &[equal_key, other]).expect_err("a different value"));
    // The stored value is compared **structurally**: `put/2` copies the key and value onto the
    // pointer-stable arena (the escape-store treatment, so a stored word can never dangle
    // across a process-heap collection), so the word read back is a distinct copy of `value`.
    // The oracle behaves the same way — `erts_debug:same(V, persistent_term:get(K))` is
    // `false` while `V =:= persistent_term:get(K)` is `true` — so identity is not the contract.
    let stored = call(&mut rt, "get", 1, &[equal_key]).expect("get(EqualKey)");
    assert!(
        beam_term::compare::eq(stored, value, rt.atoms()),
        "the rejected put_new left the stored value untouched (got {stored:?})"
    );
}

// ---------------------------------------------------------------------------------------
// The live-oracle cross-check
// ---------------------------------------------------------------------------------------
/// The golden sequence compared **value for value** against the live oracle (the recorded
/// probe in the module docs). Skips loudly without a toolchain.
#[test]
fn the_goldens_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("persistent_term") else {
        return;
    };

    // `badarg` is reported as a bare atom rather than the full `{'EXIT', {badarg, …}}`
    // stacktrace, so the two sides compare on the observable outcome.
    let expr = "\
Bad = fun(X) -> case X of {'EXIT',{badarg,_}} -> badarg; Other -> Other end end,
G = persistent_term:put(mi_k,1),
V = persistent_term:get(mi_k),
D = persistent_term:get(mi_missing,default_v),
E1 = persistent_term:erase(mi_k),
E2 = persistent_term:erase(mi_k),
PN1 = persistent_term:put_new(mi_n,1),
PN2 = persistent_term:put_new(mi_n,1),
PN3 = Bad(catch persistent_term:put_new(mi_n,2)),
G1 = Bad(catch persistent_term:get(mi_missing)),
I = persistent_term:info(),
io:format(\"~p ~p ~p ~p ~p ~p ~p ~p~n\",[G,V,D,E1,E2,PN1,PN2,PN3]),
io:format(\"missing=~p~n\",[G1]),
io:format(\"info_keys=~p~n\",[lists:sort(maps:keys(I))]),
halt(0).";
    let Some(rendered) = support::oracle_eval(&oracle, None, expr) else {
        eprintln!("SKIP the live-oracle comparison: the erl probe could not answer");
        return;
    };
    let oracle_lines: Vec<String> = rendered.lines().map(|l| l.trim().to_string()).collect();
    let mut rt = runtime();
    let k = rt.intern("mi_k");
    let n = rt.intern("mi_n");
    let missing = rt.intern("mi_missing");
    let default = rt.intern("default_v");
    let ok_atom = rt.intern("ok");
    let true_term = rt.intern("true");
    let false_term = rt.intern("false");
    let badarg = rt.intern("badarg");
    let one = make_small(1);
    let two = make_small(2);

    let put = call(&mut rt, "put", 2, &[k, one]).expect("put");
    let get = call(&mut rt, "get", 1, &[k]).expect("get");
    let defaulted = call(&mut rt, "get", 2, &[missing, default]).expect("get/2");
    let erase1 = call(&mut rt, "erase", 1, &[k]).expect("erase");
    let erase2 = call(&mut rt, "erase", 1, &[k]).expect("erase again");
    let put_new_new = call(&mut rt, "put_new", 2, &[n, one]).expect("put_new (fresh)");
    let put_new_same = call(&mut rt, "put_new", 2, &[n, one]).expect("put_new (same)");
    let put_new_diff = match call(&mut rt, "put_new", 2, &[n, two]) {
        Ok(value) => value,
        Err(error) => {
            assert_badarg(error);
            badarg
        }
    };
    let get_missing = match call(&mut rt, "get", 1, &[missing]) {
        Ok(value) => value,
        Err(error) => {
            assert_badarg(error);
            badarg
        }
    };
    let info = call(&mut rt, "info", 0, &[]).expect("info");

    let ours = vec![
        format!(
            "{} {} {} {} {} {} {} {}",
            scalar(&rt, put),
            scalar(&rt, get),
            scalar(&rt, defaulted),
            scalar(&rt, erase1),
            scalar(&rt, erase2),
            scalar(&rt, put_new_new),
            scalar(&rt, put_new_same),
            scalar(&rt, put_new_diff),
        ),
        format!("missing={}", scalar(&rt, get_missing)),
        format!("info_keys={}", info_keys(&rt, info)),
    ];
    assert_eq!(
        ours, oracle_lines,
        "the persistent_term goldens must match the live oracle"
    );
    // The `ok`/`true`/`false` outcomes are the oracle's atoms, not merely printable strings.
    assert_eq!(put, ok_atom);
    assert_eq!(erase1, true_term);
    assert_eq!(erase2, false_term);
    assert_eq!(put_new_same, ok_atom);
}

/// The **boxed**-key round trip compared **value for value** against the live oracle.
///
/// The oracle builds each key *fresh* (`list_to_tuple/1`, `list_to_binary/1`,
/// `maps:from_list/1`, `lists:seq/2`), so its `lookup` genuinely compares equal-but-distinct
/// terms; our side evaluates the same literal twice (two distinct heap words). Both sides
/// must agree on every `put`/`get`/`erase`/`erase` outcome — word identity would make each
/// `get`/`erase` fail here. Skips loudly without a toolchain.
#[test]
fn boxed_keys_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("persistent_term") else {
        return;
    };

    let expr = "\
T1 = list_to_tuple([pterm_key,1]),
T2 = list_to_tuple([pterm_key,1]),
T3 = list_to_tuple([pterm_key,1]),
PT = persistent_term:put(T1, tuple_val),
GT = persistent_term:get(T2),
ET1 = persistent_term:erase(T3),
ET2 = persistent_term:erase(T2),
B1 = list_to_binary([1,2,3]),
B2 = list_to_binary([1,2,3]),
B3 = list_to_binary([1,2,3]),
PB = persistent_term:put(B1, bin_val),
GB = persistent_term:get(B2),
EB1 = persistent_term:erase(B3),
EB2 = persistent_term:erase(B2),
M1 = maps:from_list([{a,1}]),
M2 = maps:from_list([{a,1}]),
M3 = maps:from_list([{a,1}]),
PM = persistent_term:put(M1, map_val),
GM = persistent_term:get(M2),
EM1 = persistent_term:erase(M3),
EM2 = persistent_term:erase(M2),
L1 = lists:seq(1,3),
L2 = lists:seq(1,3),
L3 = lists:seq(1,3),
PL = persistent_term:put(L1, list_val),
GL = persistent_term:get(L2),
EL1 = persistent_term:erase(L3),
EL2 = persistent_term:erase(L2),
io:format(\"~p ~p ~p ~p ~p ~p ~p ~p ~p ~p ~p ~p ~p ~p ~p ~p~n\",
          [PT,GT,ET1,ET2, PB,GB,EB1,EB2, PM,GM,EM1,EM2, PL,GL,EL1,EL2]),
halt(0).";
    let Some(rendered) = support::oracle_eval(&oracle, None, expr) else {
        eprintln!("SKIP the live-oracle boxed-key comparison: the erl probe could not answer");
        return;
    };
    let oracle_line = rendered.trim().to_string();

    let mut rt = runtime();
    let ok_atom = rt.intern("ok");
    let true_term = rt.intern("true");
    let false_term = rt.intern("false");

    // One `(literal, value atom)` per shape the oracle probe builds, in probe order.
    let cases = [
        ("{pterm_key,1}", "tuple_val"),
        ("<<1,2,3>>", "bin_val"),
        ("#{a => 1}", "map_val"),
        ("[1,2,3]", "list_val"),
    ];
    let mut tokens: Vec<String> = Vec::new();
    for (literal, value_name) in cases {
        let key = rt
            .eval(literal)
            .unwrap_or_else(|e| panic!("{literal}: {e:?}"));
        let equal = rt
            .eval(literal)
            .unwrap_or_else(|e| panic!("{literal} again: {e:?}"));
        let value = rt.intern(value_name);
        assert_ne!(
            key, equal,
            "`{literal}` twice must be two distinct heap words"
        );

        let put = call(&mut rt, "put", 2, &[key, value]).expect("put(Key, Value)");
        let got = call(&mut rt, "get", 1, &[equal]).expect("get(Equal)");
        let erased = call(&mut rt, "erase", 1, &[equal]).expect("erase(Equal)");
        let erased_again = call(&mut rt, "erase", 1, &[equal]).expect("erase(Equal) again");

        assert_eq!(put, ok_atom, "put/2 of a {literal} key is ok");
        assert_eq!(got, value, "get/1 through an equal {literal} key");
        assert_eq!(erased, true_term, "erase/1 through an equal {literal} key");
        assert_eq!(erased_again, false_term, "the {literal} key is gone");

        tokens.push(scalar(&rt, put));
        tokens.push(scalar(&rt, got));
        tokens.push(scalar(&rt, erased));
        tokens.push(scalar(&rt, erased_again));
    }
    let ours = tokens.join(" ");
    assert_eq!(
        ours, oracle_line,
        "the boxed-key persistent_term goldens must match the live oracle"
    );
    assert_eq!(
        oracle_line,
        "ok tuple_val true false ok bin_val true false ok map_val true false ok list_val true false",
        "the recorded oracle answer for the boxed-key probe changed"
    );
}

// ---------------------------------------------------------------------------------------
// Term builders + readers
// ---------------------------------------------------------------------------------------
/// The value stored under `key` in the flat map `map`, or `None` when the key is absent.
fn map_value(rt: &Runtime, map: Eterm, key: &str) -> Option<Eterm> {
    terms::map_entries(map)
        .expect("a map")
        .into_iter()
        .find(|(k, _)| rt.atom_name_of(*k) == Some(key))
        .map(|(_, value)| value)
}

/// The `info/0` map's keys, sorted — `[count, memory]` exactly as `maps:keys/1` sorts them.
fn info_keys(rt: &Runtime, map: Eterm) -> String {
    let mut keys: Vec<String> = terms::map_entries(map)
        .expect("a map")
        .into_iter()
        .map(|(k, _)| {
            rt.atom_name_of(k)
                .unwrap_or_else(|| panic!("{k:?} is not a named atom"))
                .to_string()
        })
        .collect();
    keys.sort();
    format!("[{}]", keys.join(","))
}

// ---------------------------------------------------------------------------------------
// The node-global store across a process-heap collection
// ---------------------------------------------------------------------------------------

/// The committed `pt_gc` fixture: a payload built on the process's own **young heap**,
/// stored through `persistent_term:put/2`, then churned and read back out of the store.
const PT_GC_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pt_gc.beam");

/// The fixture's payload element count (`pt_gc:payload_length/0`).
const PT_GC_PAYLOAD_LENGTH: i64 = 64;

/// The churn rounds the workload runs — well past the default 233-word nursery, so the
/// process's young heap fills and the collector really runs.
const PT_GC_ROUNDS: i64 = 2_000;

/// How long the workload may take before the test declares the runtime hung.
const PT_GC_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// **A value stored through the persistent-term path must survive a process-heap
/// collection.**
///
/// The store keeps its key and value as raw `Eterm` words. When a *scheduled* process
/// supplies them (`pt_gc:run/1` builds the payload, then calls `persistent_term:put/2`), the
/// terms live on that process's own **young heap** — the heap the collector relocates at the
/// engine's heap-admission point (`src/context/gc.rs`). A raw word kept past a collection
/// would dangle, so the value the store hands back must still be the one the process built.
///
/// `pt_gc:run/1` therefore builds the 64-element payload, stores it, churns enough
/// short-lived garbage to drive real collections, and finally returns
/// `persistent_term:get(pt_gc_key)` — the value read back **out of** the store. The payload
/// must come back intact, element by element.
///
/// Needs the system Erlang installation (the boot sequence resolves the system code path);
/// skips loudly without it.
#[test]
fn a_stored_value_survives_a_process_heap_collection() {
    let Some(_oracle) = support::require_oracle("persistent_term") else {
        return;
    };
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(PT_GC_BEAM).expect("the fixture loads");

    let before = rt.gc_stats();
    let pid = rt
        .spawn_process("pt_gc", "run", vec![make_small(PT_GC_ROUNDS)])
        .expect("the workload is spawned");

    let deadline = std::time::Instant::now() + PT_GC_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "pt_gc:run/1 did not terminate within {}s",
            PT_GC_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
    }

    // The collection really happened: without one this test would prove nothing.
    let after = rt.gc_stats();
    assert!(
        after.collections > before.collections,
        "the workload must really collect the process's own young heap \
         (before {before:?}, after {after:?})"
    );

    let value = rt
        .process_result(pid)
        .expect("the workload returned the value read back out of the store");
    support::check_gc_payload(value, PT_GC_PAYLOAD_LENGTH);
}
