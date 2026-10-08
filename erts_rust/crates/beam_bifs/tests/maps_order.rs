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

//! Integration tests pinning the **stored key order** of the flat maps this crate
//! builds: ERTS keeps a flat map's key tuple sorted by `erts_cmp_flatmap_keys`
//! (atom keys by atom index, every other pair by the exact term order), and that
//! order is observable through `maps:keys/1`, `maps:to_list/1` and the
//! `term_to_binary/1` image. An insertion-order writer would diverge.
//!
//! Every expectation is pinned against the system oracle (`erl -noshell -eval '…'`,
//! OTP 29 / erts-17.1) and asserted **structurally** with `beam_term::compare::eq`:
//! a result built on the test heap is never word-identical to an independently built
//! expected term, so raw `Eterm` equality would be a false negative.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_term` for the atom
//! table and the structural comparison).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_process::ProcessId;
use beam_term::{compare, AtomTable, Eterm};

/// The atoms every case below names. Only interned names resolve without an
/// `AtomInterner` seam, so the table is seeded up front. `a`, `b`, `c` are seeded in
/// that order, which is also their index order — the order the flat-map key
/// comparator uses for atom keys.
const ATOMS: [&str; 8] = ["true", "false", "error", "ok", "none", "a", "b", "c"];

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(ATOMS);
    atoms
}

/// Runs `f` with the real [`default_registry`] over a fresh heap and the seeded
/// atom table — no optional seams installed (these BIFs need none).
fn run<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx)
}

/// Calls `Module:Function(Args)` through the real registry and unwraps the result.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Eterm {
    reg.call(module, function, args.len() as u32, ctx, args)
        .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
}

/// A list of `elems` on the test heap.
fn list_of(ctx: &mut BifContext<'_>, elems: &[Eterm]) -> Eterm {
    ctx.list(elems).expect("a short list fits")
}

/// The list of `{K, V}` tuples for `pairs`, in the given order — the input shape of
/// `maps:from_list/1`.
fn tuples_of(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let tuples: Vec<Eterm> = pairs
        .iter()
        .map(|&(k, v)| ctx.tuple(&[k, v]).expect("a 2-tuple fits"))
        .collect();
    list_of(ctx, &tuples)
}

/// Structural (`=:=`) equality — the only sound term comparison across heaps.
fn same(ctx: &BifContext<'_>, a: Eterm, b: Eterm) -> bool {
    compare::eq(a, b, ctx.atoms())
}

#[test]
fn maps_from_list_stores_keys_in_the_oracles_canonical_order() {
    run(|reg, ctx| {
        // Oracle: maps:keys(maps:from_list([{3,a},{1,b},{2,c}])) => [1,2,3]
        //         maps:values(maps:from_list([{3,a},{1,b},{2,c}])) => [b,c,a]
        let (one, two, three) = (
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
        );
        let (a, b, c) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
            ctx.atom("c").expect("seeded"),
        );
        let pairs = tuples_of(ctx, &[(three, a), (one, b), (two, c)]);
        let map = call(reg, ctx, "maps", "from_list", &[pairs]);

        let keys = call(reg, ctx, "maps", "keys", &[map]);
        let expected_keys = list_of(ctx, &[one, two, three]);
        assert!(
            same(ctx, keys, expected_keys),
            "maps:keys/1 must store [1,2,3], not the insertion order [3,1,2]"
        );

        let values = call(reg, ctx, "maps", "values", &[map]);
        let expected_values = list_of(ctx, &[b, c, a]);
        assert!(
            same(ctx, values, expected_values),
            "the values must follow the sorted keys: [b,c,a]"
        );

        // `maps:to_list/1` reports the same stored order.
        let as_list = call(reg, ctx, "maps", "to_list", &[map]);
        let expected_list = tuples_of(ctx, &[(one, b), (two, c), (three, a)]);
        assert!(same(ctx, as_list, expected_list), "maps:to_list/1 order");

        // Every sorted key resolves to its own value, so the pairing survived.
        let got_one = call(reg, ctx, "maps", "get", &[one, map]);
        assert!(same(ctx, got_one, b), "value of key 1");
        let got_two = call(reg, ctx, "maps", "get", &[two, map]);
        assert!(same(ctx, got_two, c), "value of key 2");
        let got_three = call(reg, ctx, "maps", "get", &[three, map]);
        assert!(same(ctx, got_three, a), "value of key 3");
    });
}

#[test]
fn term_to_binary_emits_the_flat_map_in_the_oracles_key_order() {
    run(|reg, ctx| {
        // Oracle:
        //   erl -noshell -eval 'io:format("~w~n",[term_to_binary(maps:from_list([{3,a},{1,b},{2,c}]))]), halt().'
        //     => <<131,116,0,0,0,3,97,1,119,1,98,97,2,119,1,99,97,3,119,1,97>>
        // Split for readability: the `131` version byte + `116` `MAP_EXT` + a 3-pair
        // size, then `97,1` (key 1), `119,1,98` ("b"), `97,2` (key 2),
        // `119,1,99` ("c"), `97,3` (key 3), `119,1,97` ("a").
        const ORACLE_IMAGE: [i64; 20] = [
            131, 116, 0, 0, 0, 3, 97, 1, 119, 1, 98, 97, 2, 119, 1, 99, 97, 3, 119, 1,
        ];
        let (one, two, three) = (
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
        );
        let (a, b, c) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
            ctx.atom("c").expect("seeded"),
        );
        let pairs = tuples_of(ctx, &[(three, a), (one, b), (two, c)]);
        let map = call(reg, ctx, "maps", "from_list", &[pairs]);

        let image = call(reg, ctx, "erlang", "term_to_binary", &[map]);
        let bytes = call(reg, ctx, "erlang", "binary_to_list", &[image]);
        // The oracle image ends with the last key (`97,3`, emitted after the last
        // value `119,1,97`), so append the byte the readable constant stops short of.
        let expected: Vec<Eterm> = ORACLE_IMAGE
            .iter()
            .copied()
            .chain(core::iter::once(97))
            .map(|v| ctx.int(v).expect("a byte"))
            .collect();
        let expected = list_of(ctx, &expected);
        assert!(
            same(ctx, bytes, expected),
            "the MAP_EXT image must list the pairs in stored key order"
        );

        // The reordered input builds the *same* map, so the image is order-independent.
        let sorted_pairs = tuples_of(ctx, &[(one, b), (two, c), (three, a)]);
        let sorted_map = call(reg, ctx, "maps", "from_list", &[sorted_pairs]);
        let sorted_image = call(reg, ctx, "erlang", "term_to_binary", &[sorted_map]);
        assert!(same(ctx, sorted_image, image), "order-independent image");
    });
}

#[test]
fn atom_keys_order_by_atom_index_not_by_insertion_or_name() {
    run(|reg, ctx| {
        // Oracle:
        //   erl -noshell -eval 'X=list_to_atom("zzzq"), Y=list_to_atom("aaaq"),
        //     io:format("~w~n",[maps:keys(maps:from_list([{X,1},{Y,2}]))]), halt().'
        //     => [zzzq,aaaq]
        //   erl -noshell -eval 'io:format("~w~n",[maps:keys(maps:from_list([{a,1},{b,2},{c,3}]))]), halt().'
        //     => [c,b,a]
        // Both pin the key order to the *atom index* (`erts_cmp_flatmap_keys`), never
        // to the insertion order or the name. This table seeds a, b, c in that order,
        // so here the index order is [a, b, c] — the reverse of the insertion order.
        let (a, b, c) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
            ctx.atom("c").expect("seeded"),
        );
        let one = ctx.int(1).expect("fits");
        let pairs = tuples_of(ctx, &[(c, one), (b, one), (a, one)]);
        let map = call(reg, ctx, "maps", "from_list", &[pairs]);

        let keys = call(reg, ctx, "maps", "keys", &[map]);
        let expected = list_of(ctx, &[a, b, c]);
        assert!(same(ctx, keys, expected), "atom keys by atom index");
    });
}

#[test]
fn put_merge_and_remove_also_store_the_canonical_key_order() {
    run(|reg, ctx| {
        let (zero, one, two) = (
            ctx.int(0).expect("fits"),
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
        );
        let a = ctx.atom("a").expect("seeded");
        let base_pairs = tuples_of(ctx, &[(one, a), (two, a)]);
        let base = call(reg, ctx, "maps", "from_list", &[base_pairs]);

        // Inserting a key that sorts first stores it first.
        let inserted = call(reg, ctx, "maps", "put", &[zero, a, base]);
        let expected = list_of(ctx, &[zero, one, two]);
        let inserted_keys = call(reg, ctx, "maps", "keys", &[inserted]);
        assert!(same(ctx, inserted_keys, expected));

        // `maps:merge/2` re-stores the union in the same order.
        let zero_pairs = tuples_of(ctx, &[(zero, a)]);
        let zero_map = call(reg, ctx, "maps", "from_list", &[zero_pairs]);
        let merged = call(reg, ctx, "maps", "merge", &[base, zero_map]);
        let merged_keys = call(reg, ctx, "maps", "keys", &[merged]);
        assert!(same(ctx, merged_keys, expected));

        // `maps:remove/2` of an absent key leaves the canonical order untouched.
        let absent = call(reg, ctx, "maps", "remove", &[zero, base]);
        let expected_rest = list_of(ctx, &[one, two]);
        let absent_keys = call(reg, ctx, "maps", "keys", &[absent]);
        assert!(same(ctx, absent_keys, expected_rest));
    });
}
