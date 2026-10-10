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

//! The **HAMT** (non-flat, >32-pair map) cases of `erts_internal`'s map helpers
//! (`src/erts_internal.rs`): `map_next/3`, `map_to_tuple_keys/1`,
//! `map_hashmap_children/1` and `erts_debug:flat_size/1`.
//!
//! Every expectation below is pinned against the system oracle (OTP 29 / erts-17.1,
//! `erl -noshell -eval …`); with `M = maps:from_list([{N,N*10} || N <- lists:seq(1,33)])`:
//!
//! * `erts_internal:map_to_tuple_keys(M)` ⇒ `error:badarg` (a HAMT has no key tuple);
//! * `erts_internal:map_hashmap_children(M)` ⇒ the head node's 13 slot words in
//!   **reverse storage order** (the C conses each slot onto `NIL` while walking the
//!   node forward);
//! * `erts_debug:flat_size(M)` ⇒ `123`;
//! * `erts_internal:map_next(0, M, [])` ⇒ `[1, M | <the map's trailing 32 pairs>]`,
//!   the integer `1` being the continuation `maps.erl:526-542`'s `to_list_internal/1`
//!   recurses with;
//! * `erts_internal:map_next(0, M, iterator)` ⇒ a 32-triple `{K,V,Rest}` chain whose
//!   terminal `Rest` is the `[NextPath | M]` continuation (`maps:next/1`);
//! * `erts_internal:map_next(1, M, iterator)` ⇒ a 32-triple chain ending in `none`.
//!
//! The map's pair order — the order all of these are expressed in — is the oracle's
//! `maps:to_list/1` order, which [`beam_codec::term::map_pairs_storage_order`] reads
//! straight off the term; `maps:to_list/1` itself is cross-checked against it here so
//! the two implementations of the walk cannot drift apart.
//!
//! # The one documented deviation
//!
//! The oracle's `Path` is an internal trie-nibble encoding and, past the first call,
//! its element count is `min(remaining reductions / 4, size)` — a function of the
//! *process's* reduction budget, not of the arguments. This runtime therefore emits
//! the oracle's deterministic first-call cap (`HAMT_CHUNK = 32 = MAP_SMALL_MAP_LIMIT`)
//! and then one further chunk per call, with its own **integer** continuation (the
//! `is_integer(Path)` guard `maps.erl:534` needs). The **pair sequence, the order and
//! the terminal `none` are the oracle's**; only the intermediate chunk boundaries and
//! the continuation integers differ, and only above 2×32 pairs. `src/maps.rs`'s
//! `next/1` uses the same encoding, so `maps:next/1` and `maps:to_list/1` agree
//! end-to-end.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::map::{map_words_capacity, write_map};
use beam_term::prelude::signed_value;
use beam_term::{is_list, is_nil, is_small, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// The atom names the four BIFs and the expected-value builders need.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true", "false", "badarg", "none", "iterator", "badmap", "notamap", "banana", "a", "b",
    ]);
    atoms
}

/// A fresh heap and atom table with the real registry over them.
fn with_ctx<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx)
}

/// Calls a BIF through the registry, panicking with its key on a failure.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Eterm {
    reg.call(module, function, arity, ctx, args)
        .unwrap_or_else(|err| panic!("{module}:{function}/{arity} raised {err:?}"))
}

/// Builds a map with [`write_map`], which selects the representation the oracle
/// does: a flat map at ≤32 distinct keys, a hash map (HAMT) above.
fn write_map_of(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let cap = map_words_capacity(pairs.len());
    // SAFETY: `ctx.heap()` hands out `cap` contiguous, exclusively-owned words from a
    // `beam_alloc::Heap` that never reallocates and outlives every term derived from
    // it, which is exactly `write_map`'s reservation contract.
    let hp = unsafe { ctx.heap().alloc_words(cap) }.expect("the test heap has room");
    // SAFETY: `hp` points at `cap == map_words_capacity(pairs.len())` exclusive words.
    unsafe { write_map(hp, pairs, ctx.atoms()) }.expect("a well-formed map")
}

/// `#{1 => 10, 2 => 20, …, 33 => 330}` — the oracle's 33-key HAMT.
fn integer_map10(ctx: &mut BifContext<'_>) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (1..=33i64)
        .map(|i| {
            (
                ctx.int(i).expect("a small int"),
                ctx.int(i * 10).expect("a small int"),
            )
        })
        .collect();
    assert_eq!(pairs.len(), 33);
    write_map_of(ctx, &pairs)
}

/// The key/value tuple `{k, v}`.
fn pair(ctx: &mut BifContext<'_>, k: Eterm, v: Eterm) -> Eterm {
    ctx.tuple(&[k, v]).expect("the pair fits")
}

/// Structural (`=:=`) equality — the only sound comparison across heaps.
fn same(ctx: &BifContext<'_>, a: Eterm, b: Eterm) -> bool {
    beam_term::compare::eq(a, b, ctx.atoms())
}

/// Asserts the call raised a plain `badarg` (`BeamError::BadArg`).
fn assert_badarg(result: Result<Eterm>, what: &str) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected BadArg, got {other:?}"),
    }
}

/// Recovers the live reason term of a structured (`error`-class) raise.
fn structured_reason(result: Result<Eterm>, what: &str) -> Eterm {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, ExceptionClass::Error, "{what}");
            assert_eq!(raise.reason, ExitReason::Structured, "{what}");
            *raise
                .live_reason()
                .unwrap_or_else(|| panic!("{what}: no live reason attached"))
                .downcast_ref::<Eterm>()
                .unwrap_or_else(|| panic!("{what}: the live reason is not an Eterm"))
        }
        other => panic!("{what}: expected a structured raise, got {other:?}"),
    }
}

/// The `[{K, V}]` list of `pairs`, in the given order.
fn list_of_pairs(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let tuples: Vec<Eterm> = pairs
        .iter()
        .map(|&(k, v)| pair(ctx, k, v))
        .collect();
    ctx.list(&tuples).expect("a short list fits")
}

/// `maps.erl:526-542`'s `to_list_internal/1`, driven through this crate's
/// `erts_internal:map_next/3`:
///
/// ```erlang
/// to_list(Map) -> to_list_internal(erts_internal:map_next(0, Map, [])).
/// to_list_internal([Iter, Map | Acc]) when is_integer(Iter) ->
///     to_list_internal(erts_internal:map_next(Iter, Map, Acc));
/// to_list_internal(Acc) -> Acc.
/// ```
fn to_list_via_map_next(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Eterm {
    let mut path = ctx.int(0).expect("fits");
    let mut acc = NIL;
    loop {
        let step = call(reg, ctx, "erts_internal", "map_next", 3, &[path, map, acc]);
        // `[Iter, Map | Acc]` — the continuation is recognized by its integer head.
        match beam_codec::term::cons_head(step) {
            Ok(head) if is_small(head) => {
                path = head;
                let rest = beam_codec::term::cons_tail(step).expect("a consumed cons");
                acc = beam_codec::term::cons_tail(rest).expect("a consumed cons");
            }
            _ => return step,
        }
    }
}

/// Walks an iterator-mode look-ahead chain `{K0,V0,{K1,V1,…}}`, collecting the
/// `(K, V)` pairs and returning whatever the chain terminates in.
fn walk_chain(ctx: &BifContext<'_>, chain: Eterm) -> (Vec<(Eterm, Eterm)>, Eterm) {
    let _ = ctx;
    let mut pairs = Vec::new();
    let mut current = chain;
    while matches!(beam_codec::term::tuple_arity(current), Ok(3)) {
        let k = beam_codec::term::tuple_element(current, 0).expect("a triple element");
        let v = beam_codec::term::tuple_element(current, 1).expect("a triple element");
        pairs.push((k, v));
        current = beam_codec::term::tuple_element(current, 2).expect("a triple element");
    }
    (pairs, current)
}

/// The map's pairs in the stored order the oracle's `maps:to_list/1` reports.
fn stored(ctx: &BifContext<'_>, map: Eterm) -> Vec<(Eterm, Eterm)> {
    let _ = ctx;
    beam_codec::term::map_pairs_storage_order(map).expect("a map's stored order")
}

/// Asserts two pair sequences are pairwise `=:=`-equal.
fn assert_same_pairs(ctx: &BifContext<'_>, got: &[(Eterm, Eterm)], want: &[(Eterm, Eterm)], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: pair count");
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert!(
            same(ctx, g.0, w.0) && same(ctx, g.1, w.1),
            "{what}: pair {i} differs"
        );
    }
}

// ---------------------------------------------------------------------------
// erts_internal:map_to_tuple_keys/1
// ---------------------------------------------------------------------------

#[test]
fn map_to_tuple_keys_is_badarg_for_a_hash_map_and_the_key_tuple_for_a_flat_map() {
    with_ctx(|reg, ctx| {
        // `map_to_tuple_keys(M)` ⇒ `error:badarg`: the C reaches a map's keys only
        // through the flat-map head's `keys` word (`erl_map.c:3322-3323`).
        let hash = integer_map10(ctx);
        assert_badarg(
            reg.call("erts_internal", "map_to_tuple_keys", 1, ctx, &[hash]),
            "map_to_tuple_keys/1 on a hash map",
        );

        // A flat map is its own key tuple: `#{1=>10,2=>20,3=>30}` ⇒ `{1,2,3}`.
        let (i1, i2, i3, i10, i20, i30) = (
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
            ctx.int(10).expect("fits"),
            ctx.int(20).expect("fits"),
            ctx.int(30).expect("fits"),
        );
        let flat = write_map_of(ctx, &[(i1, i10), (i2, i20), (i3, i30)]);
        let got = call(reg, ctx, "erts_internal", "map_to_tuple_keys", 1, &[flat]);
        let expected = ctx.tuple(&[i1, i2, i3]).expect("fits");
        assert!(same(ctx, got, expected), "the flat map's key tuple");

        // The empty flat map is the empty tuple.
        let empty = write_map_of(ctx, &[]);
        let got = call(reg, ctx, "erts_internal", "map_to_tuple_keys", 1, &[empty]);
        let expected = ctx.tuple(&[]).expect("fits");
        assert!(same(ctx, got, expected), "#{{}}'s key tuple");

        // A non-map is the structured `{badmap, Term}` (`erl_map.c:3325`).
        let notamap = ctx.atom("notamap").expect("seeded");
        let reason = structured_reason(
            reg.call("erts_internal", "map_to_tuple_keys", 1, ctx, &[notamap]),
            "map_to_tuple_keys/1 on an atom",
        );
        let badmap = ctx.atom("badmap").expect("seeded");
        let expected = ctx.tuple(&[badmap, notamap]).expect("fits");
        assert!(same(ctx, reason, expected), "{{badmap, Term}}");
    });
}

// ---------------------------------------------------------------------------
// erts_internal:map_hashmap_children/1
// ---------------------------------------------------------------------------

/// One slot of a hash-map node, as the oracle probe's `shape/1` renders it: a leaf
/// `[K | V]` cons cell (named by its key) or a sub-node (named by its own slot count).
#[derive(Debug, PartialEq, Eq)]
enum Slot {
    Leaf(i64),
    Node(usize),
}

/// Calls `map_hashmap_children/1` on `node` and records each slot as a [`Slot`],
/// re-entering the BIF for a sub-node term (the oracle's own `shape/1` probe does the
/// same). Also appends every leaf's key to `keys` in the order the slots are walked,
/// which for the head node is the **flattened returned order**.
///
/// A leaf is the map's own `[K | V]` cons cell (so `CAR` is the key and `CDR` the
/// value, which the map under test stores as `N => N*10`); a sub-node term is not a
/// cons cell, so it is walked in turn. Panics if the result is not a proper list.
fn slot_shapes(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    node: Eterm,
    shapes: &mut Vec<Slot>,
    keys: &mut Vec<i64>,
) {
    let list = call(
        reg,
        ctx,
        "erts_internal",
        "map_hashmap_children",
        1,
        &[node],
    );
    let mut cell = list;
    while is_list(cell) {
        let slot = beam_codec::term::cons_head(cell).expect("a cons cell");
        match beam_codec::term::cons_head(slot) {
            Ok(key) if is_small(key) => {
                let value = beam_codec::term::cons_tail(slot).expect("a cons cell");
                assert_eq!(
                    signed_value(value),
                    signed_value(key) * 10,
                    "a leaf slot is the map's own [K | K*10] pair"
                );
                shapes.push(Slot::Leaf(signed_value(key)));
                keys.push(signed_value(key));
            }
            _ => {
                let mut inner = Vec::new();
                slot_shapes(reg, ctx, slot, &mut inner, keys);
                shapes.push(Slot::Node(inner.len()));
            }
        }
        cell = beam_codec::term::cons_tail(cell).expect("a cons cell");
    }
    assert!(is_nil(cell), "the result is a proper list");
}

#[test]
fn map_hashmap_children_returns_the_node_slots() {
    with_ctx(|reg, ctx| {
        // The C returns the node's own in-heap slot words (`erl_map.c:3425-3461`) —
        // every leaf cons cell and every sub-node term of the argument's node — in
        // **reverse storage order**: it conses each slot onto `NIL` while walking the
        // node forward (`while(sz--) res = CONS(hp, *ptr++, res);`), so the result is
        // the reverse of the slot sequence `access::hamt_node` reads.
        let hash = integer_map10(ctx);
        let mut shapes = Vec::new();
        let mut keys = Vec::new();
        slot_shapes(reg, ctx, hash, &mut shapes, &mut keys);

        // The oracle probe (OTP 29.1.1 / erts-17.1, recorded in `src/erts_internal.rs`;
        // re-run here for `#{N => N*10 …}`): `length(...)` ⇒ `13` and `shape/1` over the
        // slots ⇒ the sequence below — the head's 13 slots, leaves first-at-the-tail.
        assert_eq!(
            shapes,
            vec![
                Slot::Leaf(22),
                Slot::Node(4),
                Slot::Node(2),
                Slot::Node(5),
                Slot::Node(2),
                Slot::Node(4),
                Slot::Leaf(15),
                Slot::Node(3),
                Slot::Leaf(8),
                Slot::Leaf(31),
                Slot::Node(4),
                Slot::Node(2),
                Slot::Node(2),
            ],
            "the oracle's 13-slot head node for the 33-key map"
        );

        // The reverse-storage-order claim, checked against the map's own **storage**
        // traversal: `maps:to_list/1` visits the head's slots in storage order (each
        // slot's keys in that slot's storage order), so flattening the returned slots in
        // the returned order — reversing at every level — must yield that traversal
        // reversed. `beam_codec::term::map_pairs_storage_order` is the public reader of
        // that order (`map_next_walks_a_hash_map_in_list_mode_and_agrees_with_maps_to_list`
        // pins it against the oracle's `maps:to_list/1`).
        let mut stored_keys: Vec<i64> = stored(ctx, hash)
            .into_iter()
            .map(|(k, _)| signed_value(k))
            .collect();
        assert_eq!(stored_keys.len(), 33, "every map key is reached");
        stored_keys.reverse();
        assert_eq!(
            keys, stored_keys,
            "the head slots are the map's slots in reverse storage order"
        );

        // A flat head is `badarg` (`HAMT_SUBTAG_HEAD_FLATMAP`).
        let one = ctx.int(1).expect("fits");
        let flat = write_map_of(ctx, &[(one, one)]);
        assert_badarg(
            reg.call("erts_internal", "map_hashmap_children", 1, ctx, &[flat]),
            "map_hashmap_children/1 on a flat map",
        );

        // A non-map is `{badmap, Term}` — here a proper list, as in the oracle.
        let two = ctx.int(2).expect("fits");
        let list = ctx.list(&[one, two]).expect("fits");
        let reason = structured_reason(
            reg.call("erts_internal", "map_hashmap_children", 1, ctx, &[list]),
            "map_hashmap_children/1 on a list",
        );
        let badmap = ctx.atom("badmap").expect("seeded");
        let expected = ctx.tuple(&[badmap, list]).expect("fits");
        assert!(same(ctx, reason, expected), "{{badmap, [1,2]}}");
    });
}

// ---------------------------------------------------------------------------
// erts_debug:flat_size/1
// ---------------------------------------------------------------------------

#[test]
fn flat_size_counts_a_hash_map_exactly() {
    with_ctx(|reg, ctx| {
        // The HAMT node interiors are the frozen ERTS layout, so the shared
        // `beam_term::size::size_object` (the port of `copy.c:88-263`) counts them
        // exactly. Oracle: `erts_debug:flat_size(M)` ⇒ `123` for the 33-key map, the
        // value this runtime's own HAMT produces.
        let hash = integer_map10(ctx);
        assert_eq!(beam_term::size::size_object(hash), 123, "size_object(HAMT)");
        let got = call(reg, ctx, "erts_debug", "flat_size", 1, &[hash]);
        let expected = ctx.int(123).expect("fits");
        assert!(same(ctx, got, expected), "erts_debug:flat_size/1 on a HAMT");

        // A tuple is unaffected: oracle `erts_debug:flat_size({1,2,3})` ⇒ `4`.
        let (one, two, three) = (
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
        );
        let tuple = ctx.tuple(&[one, two, three]).expect("fits");
        let got = call(reg, ctx, "erts_debug", "flat_size", 1, &[tuple]);
        let expected = ctx.int(4).expect("fits");
        assert!(same(ctx, got, expected), "erts_debug:flat_size({{1,2,3}})");
    });
}

// ---------------------------------------------------------------------------
// erts_internal:map_next/3 — list mode (maps:to_list/1's walk)
// ---------------------------------------------------------------------------

#[test]
fn map_next_walks_a_hash_map_in_list_mode_and_agrees_with_maps_to_list() {
    with_ctx(|reg, ctx| {
        let map = integer_map10(ctx);
        let order = stored(ctx, map);
        assert_eq!(order.len(), 33, "the HAMT holds 33 pairs");

        // `map_next(0, M, [])` ⇒ `[NextPath, M | <the trailing 32 pairs>]`.
        let step = call(reg, ctx, "erts_internal", "map_next", 3, &[
            ctx.int(0).expect("fits"),
            map,
            NIL,
        ]);
        let next_path = beam_codec::term::cons_head(step).expect("a cons");
        assert!(
            is_small(next_path) && signed_value(next_path) == 1,
            "the continuation must be the integer 1 (33 - HAMT_CHUNK)"
        );
        let after_path = beam_codec::term::cons_tail(step).expect("a tail");
        let echoed_map = beam_codec::term::cons_head(after_path).expect("a cons");
        assert!(same(ctx, echoed_map, map), "the continuation echoes the map");
        let chunk = beam_codec::term::cons_tail(after_path).expect("a tail");
        let chunk_pairs = walk_list_pairs(ctx, chunk);
        assert_same_pairs(ctx, &chunk_pairs, &order[1..], "the trailing chunk");

        // The whole walk reproduces the stored order — the end-to-end check that
        // `maps:to_list/1` and this walk cannot drift apart.
        let walked = to_list_via_map_next(reg, ctx, map);
        let expected = list_of_pairs(ctx, &order);
        assert!(
            same(ctx, walked, expected),
            "map_next/3's list-mode walk must reproduce the stored pair order"
        );

        // `maps:to_list/1` (the sibling implementation) reports the same order.
        let from_maps = call(reg, ctx, "maps", "to_list", 1, &[map]);
        assert!(
            same(ctx, from_maps, expected),
            "maps:to_list/1 must agree with the stored order"
        );

        // A flat map still emits its whole chain, whatever the offset.
        let (one, ten, two, twenty) = (
            ctx.int(1).expect("fits"),
            ctx.int(10).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(20).expect("fits"),
        );
        let flat = write_map_of(ctx, &[(one, ten), (two, twenty)]);
        let flat_walked = to_list_via_map_next(reg, ctx, flat);
        let flat_order = stored(ctx, flat);
        let expected = list_of_pairs(ctx, &flat_order);
        assert!(
            same(ctx, flat_walked, expected),
            "a flat map's chain is the whole stored order"
        );
    });
}

/// The `{K, V}` pairs of a proper list of 2-tuples.
fn walk_list_pairs(ctx: &BifContext<'_>, list: Eterm) -> Vec<(Eterm, Eterm)> {
    let _ = ctx;
    let mut pairs = Vec::new();
    let mut current = list;
    loop {
        match beam_codec::term::cons_head(current) {
            Ok(head) => {
                let k = beam_codec::term::tuple_element(head, 0).expect("a pair key");
                let v = beam_codec::term::tuple_element(head, 1).expect("a pair value");
                pairs.push((k, v));
                current = beam_codec::term::cons_tail(current).expect("a tail");
            }
            Err(_) => return pairs,
        }
    }
}

// ---------------------------------------------------------------------------
// erts_internal:map_next/3 — iterator mode (maps:next/1's walk)
// ---------------------------------------------------------------------------

#[test]
fn map_next_walks_a_hash_map_in_iterator_mode() {
    with_ctx(|reg, ctx| {
        let map = integer_map10(ctx);
        let order = stored(ctx, map);
        let (zero, one) = (ctx.int(0).expect("fits"), ctx.int(1).expect("fits"));
        let iterator = ctx.atom("iterator").expect("seeded");
        let none = ctx.atom("none").expect("seeded");

        // `map_next(0, M, iterator)` ⇒ the first 32 pairs, terminating in `[1 | M]`.
        let chain = call(reg, ctx, "erts_internal", "map_next", 3, &[zero, map, iterator]);
        let (pairs, terminal) = walk_chain(ctx, chain);
        assert_same_pairs(ctx, &pairs, &order[..32], "(0, M, iterator) pairs");
        let next_path = beam_codec::term::cons_head(terminal).expect("the continuation cons");
        assert!(
            is_small(next_path) && signed_value(next_path) == 32,
            "the chain must end in the [NextPath | Map] continuation"
        );
        let echoed = beam_codec::term::cons_tail(terminal).expect("a tail");
        assert!(same(ctx, echoed, map), "the continuation echoes the map");

        // Resuming at that path yields the remaining pair and no further continuation.
        let resumed = call(reg, ctx, "erts_internal", "map_next", 3, &[
            ctx.int(32).expect("fits"),
            map,
            iterator,
        ]);
        let (pairs, terminal) = walk_chain(ctx, resumed);
        assert_same_pairs(ctx, &pairs, &order[32..], "(32, M, iterator) pairs");
        assert!(same(ctx, terminal, none), "(32, M, iterator) ends in none");

        // `map_next(1, M, iterator)` ⇒ the trailing 32 pairs, ending in `none`.
        let chain = call(reg, ctx, "erts_internal", "map_next", 3, &[one, map, iterator]);
        let (pairs, terminal) = walk_chain(ctx, chain);
        assert_same_pairs(ctx, &pairs, &order[1..], "(1, M, iterator) pairs");
        assert!(same(ctx, terminal, none), "(1, M, iterator) ends in none");

        // A flat map folds every pair into one chain ending in `none`.
        let (k1, v1, k2, v2) = (
            ctx.int(1).expect("fits"),
            ctx.int(10).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(20).expect("fits"),
        );
        let flat = write_map_of(ctx, &[(k1, v1), (k2, v2)]);
        let chain = call(reg, ctx, "erts_internal", "map_next", 3, &[zero, flat, iterator]);
        let (pairs, terminal) = walk_chain(ctx, chain);
        assert_same_pairs(ctx, &pairs, &stored(ctx, flat), "(0, F, iterator) pairs");
        assert!(same(ctx, terminal, none), "(0, F, iterator) ends in none");
    });
}

// ---------------------------------------------------------------------------
// erts_internal:map_next/3 — ordered iterator mode, and its error cases
// ---------------------------------------------------------------------------

#[test]
fn map_next_ordered_iterator_walks_a_hash_map_and_rejects_bad_arguments() {
    with_ctx(|reg, ctx| {
        let map = integer_map10(ctx);
        let order = stored(ctx, map);
        let iterator = ctx.atom("iterator").expect("seeded");
        let none = ctx.atom("none").expect("seeded");

        // The ordered path is the key list `maps:iterator(M, ordered)` carries: one
        // `{Key, Value, …}` triple per key, in the path's order, ending in `none`.
        // `erl_map.c:3656-3707` looks each key up in the map, whatever its kind — so
        // this now works for a HAMT too.
        let first = order[0].0;
        let last = order[32].0;
        let path = ctx.list(&[last, first]).expect("fits");
        let chain = call(reg, ctx, "erts_internal", "map_next", 3, &[path, map, iterator]);
        let (pairs, terminal) = walk_chain(ctx, chain);
        assert_same_pairs(
            ctx,
            &pairs,
            &[(last, order[32].1), (first, order[0].1)],
            "([last, first], M, iterator)",
        );
        assert!(same(ctx, terminal, none), "the ordered walk ends in none");

        // A single-key path, and the empty path (`none` itself).
        let one_key = ctx.list(&[first]).expect("fits");
        let chain = call(reg, ctx, "erts_internal", "map_next", 3, &[one_key, map, iterator]);
        let (pairs, terminal) = walk_chain(ctx, chain);
        assert_same_pairs(ctx, &pairs, &[(first, order[0].1)], "([first], M, iterator)");
        assert!(same(ctx, terminal, none), "([first], M, iterator) ends in none");
        let got = call(reg, ctx, "erts_internal", "map_next", 3, &[NIL, map, iterator]);
        assert!(same(ctx, got, none), "([], M, iterator)");

        // A key the map does not hold is `badarg` (`erl_map.c`'s ordered_badarg).
        let absent = ctx.atom("banana").expect("seeded");
        let absent_path = ctx.list(&[absent]).expect("fits");
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[absent_path, map, iterator]),
            "([banana], M, iterator)",
        );

        // The argument checks are shared with the flat-map paths: a non-map is a plain
        // `badarg`; a negative position has a huge unsigned value; a mode that is
        // neither `iterator` nor a list/NIL is `badarg`.
        let notamap = ctx.atom("notamap").expect("seeded");
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[
                ctx.int(0).expect("fits"),
                notamap,
                NIL,
            ]),
            "(0, notamap, [])",
        );
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[
                ctx.int(-1).expect("fits"),
                map,
                iterator,
            ]),
            "(-1, M, iterator)",
        );
        let banana = ctx.atom("banana").expect("seeded");
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[
                ctx.int(0).expect("fits"),
                map,
                banana,
            ]),
            "(0, M, banana)",
        );
    });
}
