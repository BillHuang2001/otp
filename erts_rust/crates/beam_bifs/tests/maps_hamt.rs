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

//! Integration tests for the whole `maps` BIF family over **both** map
//! representations — the flat map (≤ 32 distinct keys) and the HAMT hash map above
//! it — plus the `erlang:map_*` entries the module registers.
//!
//! A map is a *flat* map when it holds at most `MAP_SMALL_MAP_LIMIT` (32) distinct
//! keys and a *HAMT* above that (`erl_map.h`), and the two have different interiors:
//! a flat map stores its keys in `erts_cmp_flatmap_keys` order, a HAMT in its trie's
//! node-slot pre-order. Every order-defining reader (`keys/1`, `values/1`,
//! `to_list/1`, the `iterator/1` walk, the `fun`-taking traversals) reports that
//! **stored** order unchanged, so the HAMT half of this suite asserts the oracle's
//! exact pair order rather than a set.
//!
//! Ground truth is the system `erl` (OTP 29 / erts-17.1). The order constants below
//! and the `MAP_EXT` image are oracle output; the final test re-derives them from a
//! live `erl` at run time (skipping LOUDLY when `erl` is absent).
//!
//! These tests drive only the public API: the real [`BifRegistry`]/[`BifContext`]
//! seam plus the declared dependency crates (`beam_alloc` for the heap, `beam_term`
//! for the atom table, the structural comparison and the map readers, `beam_codec`
//! for term accessors and the `MAP_EXT` blob). Every expected map term is compared
//! **structurally** (`=:=`), because a result built on the test heap is never
//! word-identical to an independently built expected term.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, FunCaller};
use beam_codec::term::{cons_head, cons_tail, make_binary, tuple_arity, tuple_element};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, compare, is_atom, is_list, is_nil, is_small, make_atom, make_small, signed_value, AtomTable, Eterm, NIL};

/// The atoms every case below names. Only interned names resolve without an
/// `AtomInterner` seam, so the table is seeded up front. `a`, `b`, `c` are seeded in
/// that order, which is also their index order — the order the flat-map key
/// comparator uses for atom keys.
const ATOMS: [&str; 12] = [
    "true", "false", "ok", "error", "none", "a", "b", "c", "badmap", "badkey",
    "case_clause", "x",
];

/// `maps:keys/1` of `maps:from_list([{N,N} || N <- lists:seq(1,33)])` under the
/// oracle: a 33-pair map is a HAMT whose node-slot pre-order is this permutation.
const HAMT_33_ORDER: [i64; 33] = [
    18, 4, 12, 19, 29, 13, 2, 7, 31, 8, 10, 23, 9, 15, 32, 1, 25, 28, 20, 6, 11, 17, 24, 14, 33, 3,
    16, 30, 21, 5, 27, 26, 22,
];

/// `maps:keys/1` of the same map extended to 40 keys (the oracle's HAMT order grows
/// by interpolating the new keys; it is not an append).
const HAMT_40_ORDER: [i64; 40] = [
    18, 4, 34, 12, 19, 29, 13, 2, 7, 38, 31, 8, 10, 23, 9, 15, 35, 36, 32, 1, 25, 28, 20, 6, 11,
    17, 24, 37, 40, 14, 33, 3, 16, 30, 21, 5, 27, 26, 22, 39,
];

/// `term_to_binary(maps:from_list([{N,N} || N <- lists:seq(1,33)]))` — the system
/// oracle's 138-byte `MAP_EXT` image of the 33-pair HAMT. Its wire order is the
/// **reverse** of the `maps:keys/1` storage order above (the C encoder walks the
/// HAMT with a LIFO work stack; see `beam_codec`'s `encode_hashmap_pairs`).
const MAP_EXT_33: [u8; 138] = [
    131, 116, 0, 0, 0, 33, 97, 22, 97, 22, 97, 26, 97, 26, 97, 27, 97, 27, 97, 5, 97, 5, 97, 21,
    97, 21, 97, 30, 97, 30, 97, 16, 97, 16, 97, 3, 97, 3, 97, 33, 97, 33, 97, 14, 97, 14, 97, 24,
    97, 24, 97, 17, 97, 17, 97, 11, 97, 11, 97, 6, 97, 6, 97, 20, 97, 20, 97, 28, 97, 28, 97, 25,
    97, 25, 97, 1, 97, 1, 97, 32, 97, 32, 97, 15, 97, 15, 97, 9, 97, 9, 97, 23, 97, 23, 97, 10,
    97, 10, 97, 8, 97, 8, 97, 31, 97, 31, 97, 7, 97, 7, 97, 2, 97, 2, 97, 13, 97, 13, 97, 29, 97,
    29, 97, 19, 97, 19, 97, 12, 97, 12, 97, 4, 97, 4, 97, 18, 97, 18,
];

/// The oracle's `erl` expression for the 33-key HAMT corpus.
const ORACLE_33: &str = "maps:from_list([{N,N} || N <- lists:seq(1,33)])";
/// The oracle's `erl` expression for the 40-key HAMT corpus.
const ORACLE_40: &str = "maps:from_list([{N,N} || N <- lists:seq(1,40)])";
/// The 33-key HAMT with the values scaled by 10 (pins the key↔value pairing).
const ORACLE_SCALED: &str = "maps:from_list([{N,N*10} || N <- lists:seq(1,33)])";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(ATOMS);
    atoms
}

/// Runs `f` with the real [`default_registry`] over a fresh heap and the seeded
/// atom table — no optional seams installed (the plain BIFs here need none).
///
/// The heap is sized for the HAMT battery: a 40-key HAMT is ~900 words and several
/// maps plus their pair lists are live at once.
fn run<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 18).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx)
}

/// Calls `Module:Function` through the real registry, returning the raw `Result`
/// (the failure cases assert on the raise itself).
fn invoke(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Result<Eterm> {
    reg.call(module, function, args.len() as u32, ctx, args)
}

/// Calls `Module:Function` through the real registry and unwraps the result.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Eterm {
    invoke(reg, ctx, module, function, args)
        .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
}

/// The small integer `v` as a term. Every value this suite uses fits the 60-bit
/// immediate representation, so no heap is needed (matching [`BifContext::int`]).
fn int(v: i64) -> Eterm {
    make_small(v)
}

/// The boolean atom `true`/`false` of the context's atom table.
fn boolean(ctx: &BifContext<'_>, value: bool) -> Eterm {
    ctx.boolean(value).expect("a seeded boolean atom")
}

/// The `i64` behind a small-integer term.
fn number(term: Eterm) -> i64 {
    assert!(is_small(term), "expected a small integer, got {term:?}");
    signed_value(term)
}

/// Structural (`=:=`) equality — the only sound term comparison across heaps.
fn same(ctx: &BifContext<'_>, a: Eterm, b: Eterm) -> bool {
    compare::eq(a, b, ctx.atoms())
}

/// Whether `term` is exactly the atom `name`.
fn is_atom_named(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// The elements of a proper list.
fn elements(term: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        out.push(cons_head(cur).expect("a cons head"));
        cur = cons_tail(cur).expect("a cons tail");
    }
    assert!(
        is_nil(cur),
        "expected a proper list, got a term ending in {cur:?}"
    );
    out
}

/// Whether `term` is a tuple (a `{K, V, NextIter}` look-ahead or any other tuple).
fn is_tuple(term: Eterm) -> bool {
    tuple_arity(term).is_ok()
}

/// The `(key, value)` pairs of a map in the map's **stored** order.
fn storage_pairs(term: Eterm) -> Vec<(Eterm, Eterm)> {
    beam_term::map::map_pairs_storage_order(term).expect("a map")
}

/// `is_hashmap(term)` — a HAMT, i.e. a map above `MAP_SMALL_MAP_LIMIT` pairs.
fn is_hamt(term: Eterm) -> bool {
    // SAFETY: `term` is a live term word; `is_hashmap` checks the boxed tag before it
    // dereferences the pointer.
    unsafe { beam_term::is_hashmap(term) }
}

/// `is_flatmap(term)` — a flat map.
fn is_flat(term: Eterm) -> bool {
    // SAFETY: as `is_hamt`.
    unsafe { beam_term::is_flatmap(term) }
}

/// A proper list of small integers on the test heap.
fn ints_list(ctx: &mut BifContext<'_>, values: &[i64]) -> Eterm {
    let elems: Vec<Eterm> = values.iter().map(|&v| int(v)).collect();
    ctx.list(&elems).expect("a short list fits")
}

/// `maps:from_list/1` over `{K, V}` small-integer pairs.
fn map_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, pairs: &[(i64, i64)]) -> Eterm {
    let mut tuples = Vec::with_capacity(pairs.len());
    for &(k, v) in pairs {
        let key = int(k);
        let value = int(v);
        tuples.push(ctx.tuple(&[key, value]).expect("a 2-tuple fits"));
    }
    let list = ctx.list(&tuples).expect("the pair list fits");
    call(reg, ctx, "maps", "from_list", &[list])
}

/// The `{N, N}` pairs of `1..=n` — the oracle's HAMT corpus.
fn ident_pairs(n: i64) -> Vec<(i64, i64)> {
    (1..=n).map(|i| (i, i)).collect()
}

/// `maps:from_list([{N,N} || N <- lists:seq(1, n)])`.
fn ident_map(reg: &BifRegistry, ctx: &mut BifContext<'_>, n: i64) -> Eterm {
    map_of(reg, ctx, &ident_pairs(n))
}

/// `maps:keys/1` as small integers.
fn keys_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Vec<i64> {
    let keys = call(reg, ctx, "maps", "keys", &[map]);
    elements(keys).into_iter().map(number).collect()
}

/// `maps:values/1` as small integers.
fn values_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Vec<i64> {
    let values = call(reg, ctx, "maps", "values", &[map]);
    elements(values).into_iter().map(number).collect()
}

/// `maps:to_list/1` as small-integer pairs, in the map's stored order.
fn pairs_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Vec<(i64, i64)> {
    let list = call(reg, ctx, "maps", "to_list", &[map]);
    elements(list)
        .into_iter()
        .map(|pair| {
            (
                number(tuple_element(pair, 0).expect("a key")),
                number(tuple_element(pair, 1).expect("a value")),
            )
        })
        .collect()
}

/// `maps:size/1` (or `erlang:map_size/1`) as an `i64`.
fn size_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> i64 {
    number(call(reg, ctx, "maps", "size", &[map]))
}

/// Asserts `result` is the oracle's structured raise whose live reason term is
/// structurally equal to `expected`.
#[track_caller]
fn assert_structured(result: Result<Eterm>, ctx: &BifContext<'_>, expected: Eterm) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, beam_common::ExceptionClass::Error, "class");
            assert_eq!(raise.reason, beam_common::ExitReason::Structured, "reason");
            assert_eq!(raise.reason.as_atom(), None, "as_atom");
            let live = *raise
                .live_reason()
                .expect("a structured reason carries its live term")
                .downcast_ref::<Eterm>()
                .expect("the payload is the live reason term");
            assert!(
                compare::eq(live, expected, ctx.atoms()),
                "live reason differs from the expected reason tuple"
            );
        }
        other => panic!("expected a structured raise, got {other:?}"),
    }
}

/// Asserts `result` is the oracle's structured `error:{Tag, Value}` raise.
#[track_caller]
fn assert_reason(result: Result<Eterm>, ctx: &mut BifContext<'_>, tag: &str, value: Eterm) {
    let tag = ctx.atom(tag).expect("a seeded tag atom");
    let expected = ctx.tuple(&[tag, value]).expect("a 2-tuple fits");
    assert_structured(result, ctx, expected);
}

/// The compact `~p` rendering of a proper list of small integers (`[18,4,…]`).
fn render_ints(term: Eterm) -> String {
    let mut out = String::from("[");
    for (i, elem) in elements(term).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&number(*elem).to_string());
    }
    out.push(']');
    out
}

/// The compact `~p` rendering of a proper list of `{K, V}` small-integer tuples.
fn render_pairs(term: Eterm) -> String {
    let mut out = String::from("[");
    for (i, elem) in elements(term).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let key = number(tuple_element(*elem, 0).expect("a key"));
        let value = number(tuple_element(*elem, 1).expect("a value"));
        out.push_str(&format!("{{{key},{value}}}"));
    }
    out.push(']');
    out
}

/// The compact `~p` rendering of an atom (only `error` occurs below).
fn render_atom(ctx: &BifContext<'_>, term: Eterm) -> String {
    assert!(is_atom(term), "expected an atom, got {term:?}");
    ctx.atoms()
        .name(atom_val(term))
        .expect("the atom is interned")
        .to_string()
}

// ---------------------------------------------------------------------------
// The flat-map suite (moved out of the in-module `#[cfg(test)]` module)
// ---------------------------------------------------------------------------

#[test]
fn new_builds_the_empty_map() {
    run(|reg, ctx| {
        let m = call(reg, ctx, "maps", "new", &[]);
        assert!(is_flat(m) && !is_hamt(m));
        assert!(storage_pairs(m).is_empty());
        assert_eq!(size_of(reg, ctx, m), 0);
        let keys = call(reg, ctx, "maps", "keys", &[m]);
        assert!(same(ctx, keys, NIL));
        let values = call(reg, ctx, "maps", "values", &[m]);
        assert!(same(ctx, values, NIL));
        let pairs = call(reg, ctx, "maps", "to_list", &[m]);
        assert!(same(ctx, pairs, NIL));
    });
}

#[test]
fn from_list_and_to_list_round_trip() {
    run(|reg, ctx| {
        let (one, two, a, b) = (
            int(1),
            int(2),
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
        );
        let p1 = ctx.tuple(&[one, a]).expect("fits");
        let p2 = ctx.tuple(&[two, b]).expect("fits");
        let list = ctx.list(&[p1, p2]).expect("fits");

        let m = call(reg, ctx, "maps", "from_list", &[list]);
        assert!(is_flat(m));
        assert_eq!(keys_of(reg, ctx, m), vec![1, 2]);
        assert_eq!(
            storage_pairs(m).into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            vec![a, b]
        );

        let back = call(reg, ctx, "maps", "to_list", &[m]);
        let elems = elements(back);
        assert_eq!(elems.len(), 2);
        assert!(same(ctx, tuple_element(elems[0], 0).expect("key"), one));
        assert!(same(ctx, tuple_element(elems[0], 1).expect("value"), a));
        assert!(same(ctx, tuple_element(elems[1], 0).expect("key"), two));
        assert!(same(ctx, tuple_element(elems[1], 1).expect("value"), b));
    });
}

#[test]
fn from_list_duplicate_key_later_wins() {
    run(|reg, ctx| {
        let one = int(1);
        let (a, b) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
        );
        let p1 = ctx.tuple(&[one, a]).expect("fits");
        let p2 = ctx.tuple(&[one, b]).expect("fits");
        let list = ctx.list(&[p1, p2]).expect("fits");

        // Oracle: `maps:from_list([{1,a},{1,b}])` ⇒ `#{1 => b}`.
        let m = call(reg, ctx, "maps", "from_list", &[list]);
        assert_eq!(size_of(reg, ctx, m), 1);
        assert_eq!(keys_of(reg, ctx, m), vec![1]);
        assert_eq!(
            storage_pairs(m).into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            vec![b]
        );
    });
}

#[test]
fn from_list_stores_keys_in_the_canonical_flatmap_key_order() {
    run(|reg, ctx| {
        // Oracle: `maps:keys(maps:from_list([{3,a},{1,b},{2,c}]))` => [1,2,3]
        let (a, b, c) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
            ctx.atom("c").expect("seeded"),
        );
        let p3 = ctx.tuple(&[int(3), a]).expect("fits");
        let p1 = ctx.tuple(&[int(1), b]).expect("fits");
        let p2 = ctx.tuple(&[int(2), c]).expect("fits");
        let list = ctx.list(&[p3, p1, p2]).expect("fits");

        let m = call(reg, ctx, "maps", "from_list", &[list]);
        assert_eq!(keys_of(reg, ctx, m), vec![1, 2, 3]);
        assert_eq!(
            storage_pairs(m).into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            vec![b, c, a]
        );
    });
}

#[test]
fn from_list_rejects_non_list_and_malformed_pairs() {
    run(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");

        assert!(matches!(
            invoke(reg, ctx, "maps", "from_list", &[make_small(1)]),
            Err(BeamError::BadArg)
        ));

        let not_a_pair = ctx.list(&[make_small(1)]).expect("fits");
        assert!(matches!(
            invoke(reg, ctx, "maps", "from_list", &[not_a_pair]),
            Err(BeamError::BadArg)
        ));

        let three = ctx
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("fits");
        let has_three = ctx.list(&[three]).expect("fits");
        assert!(matches!(
            invoke(reg, ctx, "maps", "from_list", &[has_three]),
            Err(BeamError::BadArg)
        ));

        let improper = ctx.cons(make_small(1), a).expect("fits");
        assert!(matches!(
            invoke(reg, ctx, "maps", "from_list", &[improper]),
            Err(BeamError::BadArg)
        ));
    });
}

#[test]
fn get_2_and_get_3_report_badkey_and_badmap() {
    run(|reg, ctx| {
        let one = int(1);
        let m = map_of(reg, ctx, &[(1, 2)]);

        assert_eq!(number(call(reg, ctx, "maps", "get", &[one, m])), 2);
        //   erl -noshell -eval 'try maps:get(9,#{1=>2}) catch C:R ->
        //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badkey,9}
        assert_reason(
            invoke(reg, ctx, "maps", "get", &[int(9), m]),
            ctx,
            "badkey",
            int(9),
        );
        assert_eq!(number(call(reg, ctx, "maps", "get", &[int(9), m, int(7)])), 7);
        assert_eq!(number(call(reg, ctx, "maps", "get", &[one, m, int(7)])), 2);

        //   erl -noshell -eval 'try maps:get(1,9) catch C:R ->
        //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,9}
        assert_reason(
            invoke(reg, ctx, "maps", "get", &[one, make_small(9)]),
            ctx,
            "badmap",
            make_small(9),
        );
        assert_reason(
            invoke(reg, ctx, "maps", "get", &[one, make_small(9), int(7)]),
            ctx,
            "badmap",
            make_small(9),
        );
    });
}

#[test]
fn key_identity_is_exact_so_one_is_not_one_point_zero() {
    run(|reg, ctx| {
        let one = int(1);
        let a = ctx.atom("a").expect("seeded");
        let float_one = ctx.make_float(1.0).expect("fits");
        let m = map_of(reg, ctx, &[]);
        let m = call(reg, ctx, "maps", "put", &[float_one, a, m]);

        let got = call(reg, ctx, "maps", "get", &[float_one, m]);
        assert!(same(ctx, got, a));
        assert_reason(
            invoke(reg, ctx, "maps", "get", &[one, m]),
            ctx,
            "badkey",
            one,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "update", &[one, a, m]),
            ctx,
            "badkey",
            one,
        );
        let got = call(reg, ctx, "maps", "is_key", &[one, m]);
        assert!(same(ctx, got, boolean(ctx, false)));
        let got = call(reg, ctx, "maps", "is_key", &[float_one, m]);
        assert!(same(ctx, got, boolean(ctx, true)));
        // Both keys coexist in one map, exactly as in the oracle.
        let two_keys = call(reg, ctx, "maps", "put", &[one, a, m]);
        assert_eq!(size_of(reg, ctx, two_keys), 2);
    });
}

#[test]
fn find_2_returns_ok_or_error() {
    run(|reg, ctx| {
        let one = int(1);
        let m = map_of(reg, ctx, &[(1, 2)]);

        let hit = call(reg, ctx, "maps", "find", &[one, m]);
        assert!(is_tuple(hit));
        assert!(same(ctx, tuple_element(hit, 0).expect("tag"), ctx.ok().expect("fits")));
        assert_eq!(number(tuple_element(hit, 1).expect("value")), 2);

        let miss = call(reg, ctx, "maps", "find", &[int(9), m]);
        assert!(is_atom_named(ctx, miss, "error"));

        assert_reason(
            invoke(reg, ctx, "maps", "find", &[one, make_small(9)]),
            ctx,
            "badmap",
            make_small(9),
        );
    });
}

#[test]
fn put_update_remove_and_take() {
    run(|reg, ctx| {
        let (one, two, nine) = (int(1), int(2), int(9));
        let m = map_of(reg, ctx, &[(1, 2)]);

        let replaced = call(reg, ctx, "maps", "put", &[one, nine, m]);
        assert_eq!(values_of(reg, ctx, replaced), vec![9]);
        assert_eq!(values_of(reg, ctx, m), vec![2]);

        let inserted = call(reg, ctx, "maps", "put", &[nine, one, m]);
        assert_eq!(keys_of(reg, ctx, inserted), vec![1, 9]);

        let updated = call(reg, ctx, "maps", "update", &[one, nine, m]);
        assert_eq!(values_of(reg, ctx, updated), vec![9]);
        assert_reason(
            invoke(reg, ctx, "maps", "update", &[nine, one, m]),
            ctx,
            "badkey",
            nine,
        );

        let removed = call(reg, ctx, "maps", "remove", &[one, m]);
        assert!(keys_of(reg, ctx, removed).is_empty());
        let absent = call(reg, ctx, "maps", "remove", &[int(7), m]);
        assert_eq!(keys_of(reg, ctx, absent), vec![1]);

        let taken = call(reg, ctx, "maps", "take", &[one, m]);
        assert_eq!(number(tuple_element(taken, 0).expect("value")), 2);
        assert!(keys_of(reg, ctx, tuple_element(taken, 1).expect("rest")).is_empty());
        let taken_miss = call(reg, ctx, "maps", "take", &[nine, m]);
        assert!(is_atom_named(ctx, taken_miss, "error"));

        for bad in [
            invoke(reg, ctx, "maps", "put", &[one, two, nine]),
            invoke(reg, ctx, "maps", "remove", &[one, nine]),
        ] {
            assert_reason(bad, ctx, "badmap", nine);
        }
    });
}

#[test]
fn keys_values_size_and_is_key() {
    run(|reg, ctx| {
        let one = int(1);
        let a = ctx.atom("a").expect("seeded");
        let m = map_of(reg, ctx, &[(1, 10), (2, 20)]);

        assert_eq!(keys_of(reg, ctx, m), vec![1, 2]);
        assert_eq!(values_of(reg, ctx, m), vec![10, 20]);
        assert_eq!(size_of(reg, ctx, m), 2);
        let got = call(reg, ctx, "maps", "is_key", &[one, m]);
        assert!(same(ctx, got, boolean(ctx, true)));
        let got = call(reg, ctx, "maps", "is_key", &[a, m]);
        assert!(same(ctx, got, boolean(ctx, false)));

        for bad in [one, a] {
            assert_reason(invoke(reg, ctx, "maps", "keys", &[bad]), ctx, "badmap", bad);
            assert_reason(invoke(reg, ctx, "maps", "values", &[bad]), ctx, "badmap", bad);
            assert_reason(invoke(reg, ctx, "maps", "size", &[bad]), ctx, "badmap", bad);
        }
        assert_reason(invoke(reg, ctx, "maps", "is_key", &[one, one]), ctx, "badmap", one);
    });
}

#[test]
fn merge_with_and_without() {
    run(|reg, ctx| {
        let one = int(1);
        let c = ctx.atom("c").expect("seeded");
        let m1 = map_of(reg, ctx, &[(1, 10), (2, 20)]);
        let m2 = map_of(reg, ctx, &[(2, 99), (9, 90)]);

        let merged = call(reg, ctx, "maps", "merge", &[m1, m2]);
        assert_eq!(keys_of(reg, ctx, merged), vec![1, 2, 9]);
        assert_eq!(values_of(reg, ctx, merged), vec![10, 99, 90]);
        assert_reason(
            invoke(reg, ctx, "maps", "merge", &[m1, one]),
            ctx,
            "badmap",
            one,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "merge", &[one, m1]),
            ctx,
            "badmap",
            one,
        );

        let wanted = ints_list(ctx, &[1, 9]);
        let kept = call(reg, ctx, "maps", "with", &[wanted, m1]);
        assert_eq!(keys_of(reg, ctx, kept), vec![1]);
        let empty = ctx.list(&[]).expect("fits");
        let none_kept = call(reg, ctx, "maps", "with", &[empty, m1]);
        assert!(keys_of(reg, ctx, none_kept).is_empty());

        let drop = ints_list(ctx, &[1]);
        let rest = call(reg, ctx, "maps", "without", &[drop, m1]);
        assert_eq!(keys_of(reg, ctx, rest), vec![2]);

        assert!(matches!(
            invoke(reg, ctx, "maps", "with", &[c, m1]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            invoke(reg, ctx, "maps", "without", &[c, m1]),
            Err(BeamError::BadArg)
        ));
        assert_reason(
            invoke(reg, ctx, "maps", "with", &[empty, c]),
            ctx,
            "badmap",
            c,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "without", &[empty, c]),
            ctx,
            "badmap",
            c,
        );
    });
}

/// `maps:iterator/1` is `[0 | Map]`; `maps:next/1` walks it and `none` terminates.
#[test]
fn iterator_is_path_and_map_and_next_walks_it() {
    run(|reg, ctx| {
        let (one, two) = (int(1), int(2));
        let (a, b) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
        );
        let m = map_of(reg, ctx, &[(1, 0), (2, 0)]);
        let m = call(reg, ctx, "maps", "put", &[one, a, m]);
        let m = call(reg, ctx, "maps", "put", &[two, b, m]);

        // Oracle: `maps:iterator(#{1=>a,2=>b})` ⇒ `[0 | Map]`.
        let iter = call(reg, ctx, "maps", "iterator", &[m]);
        assert!(is_list(iter));
        assert_eq!(number(cons_head(iter).expect("a path")), 0);
        assert!(same(ctx, cons_tail(iter).expect("a map"), m));

        // Oracle: `maps:next([0|Map])` ⇒ `{1,a,{2,b,none}}`.
        let first = call(reg, ctx, "maps", "next", &[iter]);
        assert!(same(ctx, tuple_element(first, 0).expect("key"), one));
        assert!(same(ctx, tuple_element(first, 1).expect("value"), a));
        let second = tuple_element(first, 2).expect("a continuation");
        assert!(same(ctx, tuple_element(second, 0).expect("key"), two));
        assert!(is_atom_named(
            ctx,
            tuple_element(second, 2).expect("the terminal"),
            "none"
        ));

        // A path beyond the map size is `badarg` (path == n is still accepted).
        let at_bound = ctx.cons(make_small(2), m).expect("fits");
        let past_bound = ctx.cons(make_small(3), m).expect("fits");
        assert!(invoke(reg, ctx, "maps", "next", &[at_bound]).is_ok());
        assert!(matches!(
            invoke(reg, ctx, "maps", "next", &[past_bound]),
            Err(BeamError::BadArg)
        ));
        // `next` of a `{K,V,I}` look-ahead is the identity; `none` is `none`.
        let again = call(reg, ctx, "maps", "next", &[first]);
        assert!(same(ctx, again, first));
        let none = ctx.atom("none").expect("seeded");
        let none_again = call(reg, ctx, "maps", "next", &[none]);
        assert!(same(ctx, none_again, none));
        // Malformed iterators are `badarg` (a bare list, a non-map tail, a non-list
        // path, a non-tuple/non-`none`).
        for bad in [
            make_small(5),
            NIL,
            ctx.cons(make_small(0), make_small(5)).expect("fits"),
            ctx.cons(a, m).expect("fits"),
            ctx.list(&[make_small(1)]).expect("fits"),
        ] {
            assert!(
                matches!(invoke(reg, ctx, "maps", "next", &[bad]), Err(BeamError::BadArg)),
                "{bad:?}"
            );
        }

        // The ordered (list-path) form walks the path's keys.
        let path = ctx.list(&[two, one]).expect("fits");
        let ordered_iter = ctx.cons(path, m).expect("fits");
        let ordered = call(reg, ctx, "maps", "next", &[ordered_iter]);
        assert!(same(ctx, tuple_element(ordered, 0).expect("key"), two));
        assert!(same(ctx, tuple_element(ordered, 1).expect("value"), b));
        let tail = tuple_element(ordered, 2).expect("a continuation");
        assert!(same(ctx, tuple_element(tail, 0).expect("key"), one));
        assert!(is_atom_named(
            ctx,
            tuple_element(tail, 2).expect("the terminal"),
            "none"
        ));
        // An absent key or an improper path tail is `badarg`.
        let bad_path = ctx.list(&[two, ctx.atom("c").expect("seeded")]).expect("fits");
        let it = ctx.cons(bad_path, m).expect("fits");
        assert!(matches!(
            invoke(reg, ctx, "maps", "next", &[it]),
            Err(BeamError::BadArg)
        ));
        let improper = ctx.cons(two, one).expect("fits");
        let it = ctx.cons(improper, m).expect("fits");
        assert!(matches!(
            invoke(reg, ctx, "maps", "next", &[it]),
            Err(BeamError::BadArg)
        ));

        assert_reason(
            invoke(reg, ctx, "maps", "iterator", &[make_small(1)]),
            ctx,
            "badmap",
            make_small(1),
        );
    });
}

/// `maps:to_list/1` accepts a map **or** an iterator, and reports a bad iterator
/// as `{badmap, Iter}`.
#[test]
fn to_list_accepts_a_map_or_an_iterator() {
    run(|reg, ctx| {
        let (one, two) = (int(1), int(2));
        let a = ctx.atom("a").expect("seeded");
        let m = map_of(reg, ctx, &[(1, 10), (2, 20)]);

        let via_map = elements(call(reg, ctx, "maps", "to_list", &[m]));
        assert!(same(ctx, tuple_element(via_map[0], 0).expect("key"), one));
        assert!(same(ctx, tuple_element(via_map[1], 0).expect("key"), two));

        let iter = call(reg, ctx, "maps", "iterator", &[m]);
        let via_iter = elements(call(reg, ctx, "maps", "to_list", &[iter]));
        assert_eq!(via_iter.len(), 2);
        assert!(same(ctx, tuple_element(via_iter[0], 0).expect("key"), one));
        assert!(same(ctx, tuple_element(via_iter[1], 0).expect("key"), two));

        // `[]`, `5` and `[0|5]` are not iterators: `{badmap, Arg}`.
        for bad in [
            NIL,
            make_small(5),
            ctx.cons(make_small(0), make_small(5)).expect("fits"),
        ] {
            assert_reason(invoke(reg, ctx, "maps", "to_list", &[bad]), ctx, "badmap", bad);
        }
        // `{a,1,c}`: the third element `c` is not an iterator. Oracle ⇒
        // `error:{badmap,{a,1,c}}`.
        let bad_tuple = ctx
            .tuple(&[a, one, ctx.atom("c").expect("seeded")])
            .expect("fits");
        assert_reason(
            invoke(reg, ctx, "maps", "to_list", &[bad_tuple]),
            ctx,
            "badmap",
            bad_tuple,
        );
    });
}

/// Every `(module, function, arity)` this module contributes to the core registry.
///
/// The `register/1` entry point is crate-private, so this drives the public
/// [`default_registry`] (which calls it). The roster is the module's own one; the
/// registry's *total* size is pinned by `tests/registry.rs`.
#[test]
fn the_registry_installs_every_maps_bif() {
    let reg = default_registry();
    for (module, name, arity) in [
        ("maps", "new", 0),
        ("maps", "from_list", 1),
        ("maps", "to_list", 1),
        ("maps", "get", 2),
        ("maps", "get", 3),
        ("maps", "find", 2),
        ("maps", "put", 3),
        ("maps", "update", 3),
        ("maps", "remove", 2),
        ("maps", "take", 2),
        ("maps", "from_keys", 2),
        ("maps", "keys", 1),
        ("maps", "values", 1),
        ("maps", "size", 1),
        ("maps", "is_key", 2),
        ("maps", "merge", 2),
        ("maps", "with", 2),
        ("maps", "without", 2),
        ("maps", "iterator", 1),
        ("maps", "next", 1),
        ("maps", "map", 2),
        ("maps", "fold", 3),
        ("maps", "filter", 2),
        ("maps", "foreach", 2),
        ("erlang", "map_get", 2),
        ("erlang", "map_size", 1),
        ("erlang", "is_map_key", 2),
    ] {
        assert!(
            reg.contains(module, name, arity),
            "missing {module}:{name}/{arity}"
        );
    }
}

#[test]
fn registered_bifs_run_through_the_registry() {
    run(|reg, ctx| {
        let empty = call(reg, ctx, "maps", "new", &[]);
        assert!(storage_pairs(empty).is_empty());
        assert_eq!(
            number(call(reg, ctx, "erlang", "map_size", &[empty])),
            0
        );
        // The `erlang:map_*` entries are the very same functions.
        let m = map_of(reg, ctx, &[(1, 11)]);
        assert_eq!(number(call(reg, ctx, "erlang", "map_get", &[int(1), m])), 11);
        let got = call(reg, ctx, "erlang", "is_map_key", &[int(1), m]);
        assert!(same(ctx, got, boolean(ctx, true)));
    });
}

// ---------------------------------------------------------------------------
// The HAMT (hash map) battery
// ---------------------------------------------------------------------------

/// A 33-key map is a real HAMT: every reader and every producer must handle it, and
/// the readers must report the oracle's exact node-slot order.
#[test]
fn a_thirty_three_key_map_is_a_hamt_in_the_oracles_storage_order() {
    run(|reg, ctx| {
        let m = ident_map(reg, ctx, 33);
        assert!(is_hamt(m), "33 keys must build a HAMT");
        assert!(!is_flat(m));
        assert_eq!(size_of(reg, ctx, m), 33);
        assert_eq!(keys_of(reg, ctx, m), HAMT_33_ORDER.to_vec());
        // The ident map's values are its keys, so `values/1` reports the same order.
        assert_eq!(values_of(reg, ctx, m), HAMT_33_ORDER.to_vec());
        assert_eq!(
            pairs_of(reg, ctx, m),
            HAMT_33_ORDER.iter().map(|&k| (k, k)).collect::<Vec<_>>()
        );
        assert_eq!(storage_pairs(m).len(), 33);
        // `erlang:map_size/1` agrees with `maps:size/1`.
        assert_eq!(number(call(reg, ctx, "erlang", "map_size", &[m])), 33);
    });
}

/// The 40-key corpus pins the order for a second HAMT shape (the new keys are
/// interpolated, not appended).
#[test]
fn a_forty_key_map_matches_the_oracles_storage_order() {
    run(|reg, ctx| {
        let m = ident_map(reg, ctx, 40);
        assert!(is_hamt(m));
        assert_eq!(size_of(reg, ctx, m), 40);
        assert_eq!(keys_of(reg, ctx, m), HAMT_40_ORDER.to_vec());
        assert_eq!(
            pairs_of(reg, ctx, m),
            HAMT_40_ORDER.iter().map(|&k| (k, k)).collect::<Vec<_>>()
        );
    });
}

#[test]
fn hamt_readers_get_find_is_key_and_size() {
    run(|reg, ctx| {
        let m = ident_map(reg, ctx, 33);

        assert_eq!(number(call(reg, ctx, "maps", "get", &[int(17), m])), 17);
        assert_eq!(
            number(call(reg, ctx, "maps", "get", &[int(33), m, int(-1)])),
            33
        );
        // An absent key: `{badkey, K}` for `get/2`, the `-1` default for `get/3`.
        assert_reason(
            invoke(reg, ctx, "maps", "get", &[int(34), m]),
            ctx,
            "badkey",
            int(34),
        );
        assert_eq!(
            number(call(reg, ctx, "maps", "get", &[int(34), m, int(-1)])),
            -1
        );

        let hit = call(reg, ctx, "maps", "find", &[int(22), m]);
        assert_eq!(number(tuple_element(hit, 1).expect("value")), 22);
        let miss = call(reg, ctx, "maps", "find", &[int(34), m]);
        assert!(is_atom_named(ctx, miss, "error"));

        for key in [1, 17, 33] {
            let got = call(reg, ctx, "maps", "is_key", &[int(key), m]);
            assert!(same(ctx, got, boolean(ctx, true)));
        }
        let got = call(reg, ctx, "maps", "is_key", &[int(34), m]);
        assert!(same(ctx, got, boolean(ctx, false)));
        // `erlang:is_map_key/2` mirrors `maps:is_key/2`.
        let got = call(reg, ctx, "erlang", "is_map_key", &[int(17), m]);
        assert!(same(ctx, got, boolean(ctx, true)));
    });
}

#[test]
fn hamt_producers_put_update_remove_take_merge_with_without() {
    run(|reg, ctx| {
        let m = ident_map(reg, ctx, 33);

        // `put/3` of an existing key replaces its value and keeps the order.
        let replaced = call(reg, ctx, "maps", "put", &[int(17), int(-1), m]);
        assert!(is_hamt(replaced));
        assert_eq!(keys_of(reg, ctx, replaced), HAMT_33_ORDER.to_vec());
        assert_eq!(
            number(call(reg, ctx, "maps", "get", &[int(17), replaced])),
            -1
        );

        // `put/3` of a 34th key stays a HAMT and keeps the oracle's order.
        let grown = call(reg, ctx, "maps", "put", &[int(99), int(99), m]);
        assert!(is_hamt(grown));
        assert_eq!(size_of(reg, ctx, grown), 34);
        let expected_99 = {
            let mut keys = HAMT_33_ORDER.to_vec();
            let at = keys.iter().position(|&k| k == 7).expect("7 is present") + 1;
            keys.insert(at, 99);
            keys
        };
        assert_eq!(keys_of(reg, ctx, grown), expected_99);

        // `update/3` on a present key, `{badkey, K}` on an absent one.
        let updated = call(reg, ctx, "maps", "update", &[int(33), int(-2), m]);
        assert_eq!(keys_of(reg, ctx, updated), HAMT_33_ORDER.to_vec());
        assert_eq!(
            number(call(reg, ctx, "maps", "get", &[int(33), updated])),
            -2
        );
        assert_reason(
            invoke(reg, ctx, "maps", "update", &[int(34), int(0), m]),
            ctx,
            "badkey",
            int(34),
        );

        // `remove/2` of a present key drops exactly one pair; of an absent key, nothing.
        let gone = call(reg, ctx, "maps", "remove", &[int(17), m]);
        assert_eq!(size_of(reg, ctx, gone), 32);
        let still = call(reg, ctx, "maps", "is_key", &[int(17), gone]);
        assert!(same(ctx, still, boolean(ctx, false)));
        let untouched = call(reg, ctx, "maps", "remove", &[int(34), m]);
        assert_eq!(size_of(reg, ctx, untouched), 33);
        assert_eq!(keys_of(reg, ctx, untouched), HAMT_33_ORDER.to_vec());

        // `take/2` returns `{Value, Rest}`; an absent key is the atom `error`.
        let taken = call(reg, ctx, "maps", "take", &[int(22), m]);
        assert_eq!(number(tuple_element(taken, 0).expect("value")), 22);
        let rest = tuple_element(taken, 1).expect("the rest");
        assert_eq!(size_of(reg, ctx, rest), 32);
        let miss = call(reg, ctx, "maps", "take", &[int(34), m]);
        assert!(is_atom_named(ctx, miss, "error"));

        // `merge/2`: `Map2`'s value wins on a shared key, and the union re-orders.
        let add_99 = map_of(reg, ctx, &[(99, 99)]);
        let merged = call(reg, ctx, "maps", "merge", &[m, add_99]);
        assert_eq!(keys_of(reg, ctx, merged), expected_99);
        let overwrite = map_of(reg, ctx, &[(17, -3)]);
        let merged = call(reg, ctx, "maps", "merge", &[m, overwrite]);
        assert_eq!(size_of(reg, ctx, merged), 33);
        assert_eq!(
            number(call(reg, ctx, "maps", "get", &[int(17), merged])),
            -3
        );

        // `with/2` and `without/2` filter the pair set (and may drop below the limit,
        // which rebuilds a flat map — checked by the boundary test).
        let wanted = ints_list(ctx, &[1, 2, 3]);
        let kept = call(reg, ctx, "maps", "with", &[wanted, m]);
        assert_eq!(keys_of(reg, ctx, kept), vec![1, 2, 3]);
        let all_keys = ints_list(ctx, &(1..=33).collect::<Vec<_>>());
        let kept_hamt = call(reg, ctx, "maps", "with", &[all_keys, m]);
        assert!(is_hamt(kept_hamt));
        assert_eq!(keys_of(reg, ctx, kept_hamt), HAMT_33_ORDER.to_vec());
        let drop = ints_list(ctx, &[1, 2, 3]);
        let rest = call(reg, ctx, "maps", "without", &[drop, m]);
        assert_eq!(size_of(reg, ctx, rest), 30);
    });
}

/// `maps:from_keys/2` over 40 keys builds the same HAMT shape as `from_list/1`.
#[test]
fn hamt_from_keys_builds_the_oracle_order() {
    run(|reg, ctx| {
        let keys = ints_list(ctx, &(1..=40).collect::<Vec<_>>());
        let value = ctx.atom("x").expect("seeded");
        let m = call(reg, ctx, "maps", "from_keys", &[keys, value]);

        assert!(is_hamt(m));
        assert_eq!(size_of(reg, ctx, m), 40);
        assert_eq!(keys_of(reg, ctx, m), HAMT_40_ORDER.to_vec());
        let values = elements(call(reg, ctx, "maps", "values", &[m]));
        assert_eq!(values.len(), 40);
        assert!(values.iter().all(|&v| same(ctx, v, value)));
        // A duplicate key collapses: 41 keys, 40 distinct.
        let with_dup = ints_list(
            ctx,
            &(1..=40).chain(core::iter::once(40)).collect::<Vec<_>>(),
        );
        let deduped = call(reg, ctx, "maps", "from_keys", &[with_dup, value]);
        assert_eq!(size_of(reg, ctx, deduped), 40);
        // A non-list key list is `badarg`.
        assert!(matches!(
            invoke(reg, ctx, "maps", "from_keys", &[make_small(1), value]),
            Err(BeamError::BadArg)
        ));
    });
}

/// The representation switches on the **distinct** key count, not on the input
/// length: at 32 pairs a map is flat, at 33 a HAMT — and a drop back to 32 pairs
/// rebuilds the flat map.
#[test]
fn the_flat_hamt_boundary_is_the_distinct_key_count() {
    run(|reg, ctx| {
        let flat = ident_map(reg, ctx, 32);
        assert!(is_flat(flat) && !is_hamt(flat), "32 keys must be a flat map");
        assert_eq!(keys_of(reg, ctx, flat), (1..=32).collect::<Vec<_>>());

        let hamt = ident_map(reg, ctx, 33);
        assert!(is_hamt(hamt), "33 keys must be a HAMT");

        // 33 input pairs with one duplicate key hold 32 distinct keys ⇒ flat map.
        let mut pairs = ident_pairs(32);
        pairs.push((32, 99));
        let deduped = map_of(reg, ctx, &pairs);
        assert!(is_flat(deduped));
        assert_eq!(size_of(reg, ctx, deduped), 32);
        assert_eq!(keys_of(reg, ctx, deduped), (1..=32).collect::<Vec<_>>());
        assert_eq!(
            number(call(reg, ctx, "maps", "get", &[int(32), deduped])),
            99,
            "the later duplicate wins"
        );

        // Removing one key from the 33-key HAMT leaves 32 pairs ⇒ flat again, with
        // the flat map's ascending key order.
        let shrunk = call(reg, ctx, "maps", "remove", &[int(33), hamt]);
        assert!(is_flat(shrunk) && !is_hamt(shrunk));
        assert_eq!(size_of(reg, ctx, shrunk), 32);
        assert_eq!(keys_of(reg, ctx, shrunk), (1..=32).collect::<Vec<_>>());

        // Adding a 33rd key to a 32-pair flat map ⇒ a HAMT.
        let grown = call(reg, ctx, "maps", "put", &[int(33), int(33), flat]);
        assert!(is_hamt(grown));
        assert_eq!(size_of(reg, ctx, grown), 33);
        assert_eq!(keys_of(reg, ctx, grown), HAMT_33_ORDER.to_vec());

        // The empty `maps:new/0` and a `maps:from_list([])` are the same flat map.
        let none = ctx.list(&[]).expect("fits");
        let empty = call(reg, ctx, "maps", "from_list", &[none]);
        assert!(is_flat(empty));
        assert_eq!(size_of(reg, ctx, empty), 0);
    });
}

/// Key identity stays exact (`=:=`) in a HAMT: `1` and `1.0` are distinct keys.
#[test]
fn hamt_key_identity_is_exact() {
    run(|reg, ctx| {
        let x = ctx.atom("x").expect("seeded");
        // 32 integer keys + the float key `1.0` = 33 distinct keys.
        let float_one = ctx.make_float(1.0).expect("fits");
        let float_two = ctx.make_float(2.0).expect("fits");
        // Build through `maps:from_list/1` so the float key goes through the writer.
        let mut tuples = Vec::new();
        for &(k, v) in &ident_pairs(32) {
            tuples.push(ctx.tuple(&[int(k), int(v)]).expect("fits"));
        }
        tuples.push(ctx.tuple(&[float_one, x]).expect("fits"));
        let list = ctx.list(&tuples).expect("fits");
        let m = call(reg, ctx, "maps", "from_list", &[list]);

        assert!(is_hamt(m));
        assert_eq!(size_of(reg, ctx, m), 33);
        assert_eq!(number(call(reg, ctx, "maps", "get", &[int(1), m])), 1);
        let got = call(reg, ctx, "maps", "get", &[float_one, m]);
        assert!(same(ctx, got, x), "the float key is distinct from `1`");
        let got = call(reg, ctx, "maps", "is_key", &[int(2), m]);
        assert!(same(ctx, got, boolean(ctx, true)));
        let got = call(reg, ctx, "maps", "is_key", &[float_two, m]);
        assert!(same(ctx, got, boolean(ctx, false)));
    });
}

/// The `iterator/1` → `next/1` walk yields every pair in the stored order, and
/// `to_list/1` of the iterator equals `to_list/1` of the map.
#[test]
fn hamt_iterators_walk_every_pair_in_storage_order() {
    run(|reg, ctx| {
        let m = ident_map(reg, ctx, 33);
        let iter = call(reg, ctx, "maps", "iterator", &[m]);
        assert!(is_list(iter));
        assert_eq!(number(cons_head(iter).expect("a path")), 0);
        assert!(same(ctx, cons_tail(iter).expect("a map"), m));

        // Walk: a `{K,V,Next}` look-ahead, a `[Path|Map]` continuation, then `none`.
        let mut seen: Vec<(i64, i64)> = Vec::new();
        let mut node = call(reg, ctx, "maps", "next", &[iter]);
        let mut guard = 0;
        while !is_atom(node) {
            guard += 1;
            assert!(guard <= 64, "the walk must terminate");
            if is_tuple(node) {
                let key = number(tuple_element(node, 0).expect("a key"));
                let value = number(tuple_element(node, 1).expect("a value"));
                seen.push((key, value));
                node = tuple_element(node, 2).expect("the continuation");
            } else {
                // A `[Path | Map]` continuation: ask `maps:next/1` for the next chunk.
                node = call(reg, ctx, "maps", "next", &[node]);
            }
        }
        assert!(
            is_atom_named(ctx, node, "none"),
            "the walk must terminate in `none`"
        );
        assert_eq!(
            seen,
            HAMT_33_ORDER.iter().map(|&k| (k, k)).collect::<Vec<_>>(),
            "the iterator must walk the HAMT in stored order"
        );

        // `maps:to_list/1` accepts the iterator and reports the same pairs.
        let via_iter = call(reg, ctx, "maps", "to_list", &[iter]);
        let via_map = call(reg, ctx, "maps", "to_list", &[m]);
        assert!(same(ctx, via_iter, via_map));
    });
}

// ---------------------------------------------------------------------------
// The `fun`-taking BIFs over a HAMT (the `FunCaller` seam)
// ---------------------------------------------------------------------------

/// `fun(K, V) -> K + V end`.
const KEY_PLUS_VALUE: i64 = 1;
/// `fun(_K, V, Acc) -> Acc + V end`.
const ACC_PLUS_VALUE: i64 = 2;
/// `fun(_K, V) -> V > 30 end`.
const VALUE_GT_30: i64 = 3;
/// `fun(K, _V) -> K end` — a fun whose result is neither `true` nor `false`.
const KEY: i64 = 4;
/// A fun whose body raises `badarg`; the BIF must propagate it untouched.
const RAISES: i64 = 5;

/// The mock `FunCaller`: the "fun" argument is a small-integer sentinel naming the
/// operation, and the mock supplies the behaviour (a real `beam_rt` resolves a
/// closure there). Every recorded call is `(operation, arguments)`.
struct MockFuns {
    truth: Eterm,
    falsity: Eterm,
    calls: Rc<RefCell<Vec<(i64, Vec<Eterm>)>>>,
}

impl MockFuns {
    fn new(atoms: &AtomTable) -> MockFuns {
        let atom = |name: &str| make_atom(atoms.lookup(name).expect("seeded atom"));
        MockFuns {
            truth: atom("true"),
            falsity: atom("false"),
            calls: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn boolean(&self, value: bool) -> Eterm {
        if value {
            self.truth
        } else {
            self.falsity
        }
    }
}

impl FunCaller for MockFuns {
    fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        let operation = number(fun);
        self.calls.borrow_mut().push((operation, args.to_vec()));
        match operation {
            KEY_PLUS_VALUE => Ok(make_small(number(args[0]) + number(args[1]))),
            ACC_PLUS_VALUE => Ok(make_small(number(args[2]) + number(args[1]))),
            VALUE_GT_30 => Ok(self.boolean(number(args[1]) > 30)),
            KEY => Ok(args[0]),
            RAISES => Err(BeamError::BadArg),
            other => panic!("unknown mock fun {other}"),
        }
    }
}

/// Runs `f` with the default registry over a fresh heap + seeded atom table and the
/// mock fun caller installed, handing it the recorded-call log.
fn with_mock<R>(
    f: impl FnOnce(&BifRegistry, &mut BifContext<'_>, &Rc<RefCell<Vec<(i64, Vec<Eterm>)>>>) -> R,
) -> R {
    let registry = default_registry();
    let mut heap = Heap::new(1 << 18).expect("test heap");
    let atoms = seeded_atoms();
    let mut mock = MockFuns::new(&atoms);
    let calls = Rc::clone(&mock.calls);
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_fun_caller(&mut mock);
    f(&registry, &mut ctx, &calls)
}

/// The `(key, value)` pairs the mock was called with, in visit order.
fn visited(calls: &Rc<RefCell<Vec<(i64, Vec<Eterm>)>>>) -> Vec<(i64, i64)> {
    calls
        .borrow()
        .iter()
        .map(|(_, args)| (number(args[0]), number(args[1])))
        .collect()
}

#[test]
fn hamt_fun_bifs_traverse_in_storage_order() {
    with_mock(|reg, ctx, calls| {
        let m = ident_map(reg, ctx, 33);

        // maps:map(fun(K,V) -> K+V end, M): the keys are unchanged, the values are
        // the fun's results — in the map's stored order.
        let mapped = call(reg, ctx, "maps", "map", &[make_small(KEY_PLUS_VALUE), m]);
        assert!(is_hamt(mapped));
        assert_eq!(size_of(reg, ctx, mapped), 33);
        assert_eq!(
            pairs_of(reg, ctx, mapped),
            HAMT_33_ORDER.iter().map(|&k| (k, 2 * k)).collect::<Vec<_>>()
        );
        assert_eq!(
            visited(calls),
            HAMT_33_ORDER.iter().map(|&k| (k, k)).collect::<Vec<_>>(),
            "map/2 must visit every pair once, in storage order"
        );
        calls.borrow_mut().clear();

        // maps:fold(fun(_K,V,Acc) -> Acc+V end, 0, M) => lists:sum(seq(1,33)) = 561.
        let total = call(
            reg,
            ctx,
            "maps",
            "fold",
            &[make_small(ACC_PLUS_VALUE), make_small(0), m],
        );
        assert_eq!(number(total), 561);
        assert_eq!(visited(calls).len(), 33);
        calls.borrow_mut().clear();

        // maps:filter(fun(_K,V) -> V > 30 end, M) keeps 31, 32 and 33.
        let kept = call(reg, ctx, "maps", "filter", &[make_small(VALUE_GT_30), m]);
        assert_eq!(size_of(reg, ctx, kept), 3);
        assert_eq!(keys_of(reg, ctx, kept), vec![31, 32, 33]);
        assert_eq!(visited(calls).len(), 33);
        calls.borrow_mut().clear();

        // maps:foreach/2 visits every pair and returns `ok`.
        let ok = call(reg, ctx, "maps", "foreach", &[make_small(KEY_PLUS_VALUE), m]);
        assert!(same(ctx, ok, ctx.atom("ok").expect("seeded")));
        assert_eq!(visited(calls), HAMT_33_ORDER.iter().map(|&k| (k, k)).collect::<Vec<_>>());
        calls.borrow_mut().clear();

        // A non-boolean predicate result is `error:{case_clause, Value}` for the
        // **first** pair visited — which pins the traversal order to the HAMT's.
        let first_key = HAMT_33_ORDER[0];
        assert_reason(
            invoke(reg, ctx, "maps", "filter", &[make_small(KEY), m]),
            ctx,
            "case_clause",
            int(first_key),
        );
        calls.borrow_mut().clear();

        // A fun's own failure propagates verbatim.
        match invoke(reg, ctx, "maps", "map", &[make_small(RAISES), m]) {
            Err(error) => assert!(matches!(error, BeamError::BadArg)),
            other => panic!("expected the fun's own error, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// Structured raises over a HAMT
// ---------------------------------------------------------------------------

/// A HAMT operand behaves exactly like a flat map at the error boundary: a bad map
/// is `{badmap, M}`, a missing key `{badkey, K}`, a non-boolean predicate result
/// `{case_clause, V}`.
#[test]
fn hamt_structured_raises_carry_their_reason_term() {
    run(|reg, ctx| {
        let m = ident_map(reg, ctx, 33);
        let not_a_map = ctx.tuple(&[make_small(1), make_small(2)]).expect("fits");

        for function in ["keys", "values", "size", "iterator", "to_list"] {
            assert_reason(
                invoke(reg, ctx, "maps", function, &[not_a_map]),
                ctx,
                "badmap",
                not_a_map,
            );
        }
        assert_reason(
            invoke(reg, ctx, "maps", "get", &[int(1), not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "get", &[int(1), not_a_map, int(0)]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "find", &[int(1), not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "is_key", &[int(1), not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "put", &[int(1), int(1), not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "update", &[int(1), int(1), not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "remove", &[int(1), not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "maps", "take", &[int(1), not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "erlang", "map_get", &[int(1), not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );
        assert_reason(
            invoke(reg, ctx, "erlang", "map_size", &[not_a_map]),
            ctx,
            "badmap",
            not_a_map,
        );

        // A missing key of a HAMT: `{badkey, K}`.
        for missing in [0, 34, -1] {
            assert_reason(
                invoke(reg, ctx, "maps", "get", &[int(missing), m]),
                ctx,
                "badkey",
                int(missing),
            );
            assert_reason(
                invoke(reg, ctx, "maps", "update", &[int(missing), int(0), m]),
                ctx,
                "badkey",
                int(missing),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// A decoded `MAP_EXT` image
// ---------------------------------------------------------------------------

/// A >32-key `MAP_EXT` image (produced by the system `erl`) decodes through
/// `beam_codec` into a HAMT whose pairs the whole `maps` family can read — the
/// inbound half of the `binary_to_term` → `maps:*` path `beam_rt` drives.
#[test]
fn a_thirty_three_key_map_ext_image_decodes_to_a_readable_hamt() {
    run(|reg, ctx| {
        let blob = make_binary(ctx.heap(), &MAP_EXT_33).expect("the blob fits");
        let m = call(reg, ctx, "erlang", "binary_to_term", &[blob]);

        assert!(is_hamt(m), "a 33-pair MAP_EXT image must decode to a HAMT");
        assert_eq!(size_of(reg, ctx, m), 33);
        assert_eq!(keys_of(reg, ctx, m), HAMT_33_ORDER.to_vec());
        assert_eq!(number(call(reg, ctx, "maps", "get", &[int(17), m])), 17);
        let missing = call(reg, ctx, "maps", "find", &[int(34), m]);
        assert!(is_atom_named(ctx, missing, "error"));
        // The decoded map reads exactly like the natively built one.
        let built = ident_map(reg, ctx, 33);
        let decoded_pairs = call(reg, ctx, "maps", "to_list", &[m]);
        let built_pairs = call(reg, ctx, "maps", "to_list", &[built]);
        assert!(same(ctx, decoded_pairs, built_pairs));
        // …and it re-encodes byte-for-byte, i.e. the decode kept the stored order.
        let image = call(reg, ctx, "erlang", "term_to_binary", &[m]);
        let bytes = elements(call(reg, ctx, "erlang", "binary_to_list", &[image]));
        let expected: Vec<Eterm> = MAP_EXT_33.iter().map(|&b| int(b as i64)).collect();
        assert_eq!(bytes.len(), MAP_EXT_33.len());
        assert!(
            bytes
                .iter()
                .zip(expected.iter())
                .all(|(&a, &b)| same(ctx, a, b)),
            "the decoded HAMT must re-encode to the oracle image"
        );
    });
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------

/// Whether the system `erl` runs at all (`erl -noshell -eval 'halt().'`).
fn erl_available() -> bool {
    std::process::Command::new("erl")
        .arg("-noshell")
        .arg("-eval")
        .arg("halt().")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Evaluates every expression in ONE system-`erl` process and returns each one's
/// `io:format("~p", …)` rendering with **all whitespace removed** (the oracle's
/// line wrapping is not part of the value), or `None` when `erl` could not run.
///
/// The expressions are separated by a `|` delimiter, so the renderings are split
/// back apart after the whitespace is stripped.
fn oracle_evals<S: AsRef<str>>(exprs: &[S]) -> Option<Vec<String>> {
    let mut code = String::from("G = fun(T) -> io:format(\"~p|\", [T]) end, ");
    for expr in exprs {
        code.push_str(&format!("G({}), ", expr.as_ref()));
    }
    code.push_str("halt().");
    let output = std::process::Command::new("erl")
        .arg("-noshell")
        .arg("-eval")
        .arg(&code)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let parts: Vec<&str> = compact.split('|').collect();
    if parts.len() != exprs.len() + 1 || !parts[exprs.len()].is_empty() {
        return None;
    }
    Some(parts[..exprs.len()].iter().map(|s| s.to_string()).collect())
}

/// Cross-checks the HAMT readers/writers against the live system `erl`.
///
/// Every expression is a key-order or scalar question about the 33- and 40-key
/// corpora, so a single `erl` process answers all of them. Skips loudly when `erl`
/// is unavailable.
#[test]
fn the_hamt_readers_agree_with_the_live_oracle() {
    if !erl_available() {
        eprintln!(
            "SKIP: the system `erl` oracle is unavailable or does not run; \
             the HAMT cross-check is skipped"
        );
        return;
    }

    run(|reg, ctx| {
        let m33 = ident_map(reg, ctx, 33);
        let m40 = ident_map(reg, ctx, 40);
        let scaled = map_of(
            reg,
            ctx,
            &(1..=33).map(|n| (n, n * 10)).collect::<Vec<_>>(),
        );
        let add_99 = map_of(reg, ctx, &[(99, 99)]);
        let three = ints_list(ctx, &[1, 2, 3]);
        let keys_1_40 = ints_list(ctx, &(1..=40).collect::<Vec<_>>());
        let x = ctx.atom("x").expect("seeded");
        let from_keys = call(reg, ctx, "maps", "from_keys", &[keys_1_40, x]);

        let mut got: Vec<String> = Vec::new();
        let term = call(reg, ctx, "maps", "keys", &[m33]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "keys", &[m40]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "values", &[m33]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "to_list", &[scaled]);
        got.push(render_pairs(term));
        let term = call(reg, ctx, "maps", "size", &[m40]);
        got.push(number(term).to_string());
        let term = call(reg, ctx, "maps", "get", &[int(17), m33]);
        got.push(number(term).to_string());
        let term = call(reg, ctx, "maps", "find", &[int(34), m33]);
        got.push(render_atom(ctx, term));
        let term = call(reg, ctx, "maps", "remove", &[int(33), m33]);
        let term = call(reg, ctx, "maps", "keys", &[term]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "put", &[int(99), int(99), m33]);
        let term = call(reg, ctx, "maps", "keys", &[term]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "update", &[int(33), int(99), m33]);
        let term = call(reg, ctx, "maps", "keys", &[term]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "merge", &[m33, add_99]);
        let term = call(reg, ctx, "maps", "keys", &[term]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "without", &[three, m33]);
        let term = call(reg, ctx, "maps", "keys", &[term]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "with", &[three, m33]);
        let term = call(reg, ctx, "maps", "keys", &[term]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "take", &[int(1), m33]);
        let term = call(reg, ctx, "maps", "keys", &[tuple_element(term, 1).expect("the rest")]);
        got.push(render_ints(term));
        let term = call(reg, ctx, "maps", "keys", &[from_keys]);
        got.push(render_ints(term));

        let exprs = [
            format!("maps:keys({ORACLE_33})"),
            format!("maps:keys({ORACLE_40})"),
            format!("maps:values({ORACLE_33})"),
            format!("maps:to_list({ORACLE_SCALED})"),
            format!("maps:size({ORACLE_40})"),
            format!("maps:get(17, {ORACLE_33})"),
            format!("maps:find(34, {ORACLE_33})"),
            format!("maps:keys(maps:remove(33, {ORACLE_33}))"),
            format!("maps:keys(maps:put(99, 99, {ORACLE_33}))"),
            format!("maps:keys(maps:update(33, 99, {ORACLE_33}))"),
            format!("maps:keys(maps:merge({ORACLE_33}, #{{99 => 99}}))"),
            format!("maps:keys(maps:without([1,2,3], {ORACLE_33}))"),
            format!("maps:keys(maps:with([1,2,3], {ORACLE_33}))"),
            format!("maps:keys(element(2, maps:take(1, {ORACLE_33})))"),
            String::from("maps:keys(maps:from_keys(lists:seq(1,40), x))"),
        ];
        assert_eq!(got.len(), exprs.len(), "one Rust result per oracle expression");

        let expected = oracle_evals(&exprs).unwrap_or_else(|| {
            panic!("the system `erl` failed to evaluate the HAMT expressions")
        });
        for ((expr, got), expected) in exprs.iter().zip(&got).zip(&expected) {
            assert_eq!(got, expected, "mismatch against the oracle for `{expr}`");
        }
    });
}
