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

//! The **path-copying** map writers, observed through the `maps` BIF surface.
//!
//! `maps:put/3`, `maps:update/3`, `maps:remove/2` and `maps:take/2` rewrite only the
//! trie nodes on the key's hash path and share every off-path subtree with the source
//! map by word (`beam_term::map::map_put_cow`/`map_remove_cow`; the C's
//! `erts_maps_put`, `erts_maps_update` and `hashmap_delete`). The retired route walked
//! every pair, mutated it and rebuilt the whole map through `write_map` — Θ(n log n)
//! of work and of words for one changed pair.
//!
//! # What this suite pins
//!
//! The optimisation must be **unobservable**. Every arm below therefore walks the same
//! four BIFs across the sizes where the representation changes and compares the result
//! with three independent observables:
//!
//! * content, `=:=` against a `maps:from_list/1` rebuild of the expected pairs
//!   (`beam_term::compare::eq`);
//! * `maps:to_list/1`, **order included** — a flat map stores its keys in
//!   `erts_cmp_flatmap_keys` order (atom keys by raw atom index, every other pair by
//!   the exact term order) and a HAMT in its trie's slot-traversal order, so a writer
//!   that emitted a different order would diverge here first;
//! * the `erlang:term_to_binary/1` (`MAP_EXT`) image, byte for byte.
//!
//! The **no-op identity** arms are re-asserted at every size this file walks (a `put`
//! or `update` whose value is the stored value *word*, and a `remove` of an absent
//! key, answer the input map term itself). `tests/maps_identity.rs` owns the measured
//! oracle matrix of those arms; this file only checks that each size band preserves
//! them.
//!
//! # The flat/HAMT boundary
//!
//! A map is flat at `MAP_SMALL_MAP_LIMIT` (32) or fewer distinct keys and a HAMT above
//! it (`erl_map.h`), decided by the **distinct key count**. The sizes 31, 32, 33 and 34
//! straddle that boundary in both directions: a `put` at 32 → 33 converts flat → HAMT,
//! a `remove` or `take` at 33 → 32 converts back. The conversion is observable through
//! `maps:to_list/1`'s order, which is what
//! [`a_put_at_the_limit_converts_flat_to_hamt_and_remove_converts_back`] reads.
//!
//! # Cost (host-independent)
//!
//! No wall-clock assertion appears in this file. What is pinned instead is the
//! **growth relation**: the words a `maps:put/3` reserves do not grow with the map's
//! size, while a rebuild reserves the whole map (see the *Path-copying cost* section).
//! `beam_term::map::map_put_cow_words_required` is the writer's own read-only
//! path descent, so the figure is the writer's, not a re-derivation here.
//!
//! # One atom namespace: the rebuild and the path copy agree on the stored order
//!
//! Both writers order a flat map's keys through the caller's **live** `AtomTable`:
//! `beam_term::map::write_map` (reached by `maps:from_list/1` through
//! `crate::access::make_map`) and `beam_term::map::map_put_cow` (reached by
//! `maps:put/3` through `crate::access::map_put_cow`) are handed the same `ctx.atoms()`
//! through `BifContext::atom_and_heap`. `erts_cmp_flatmap_keys` compares two *atom* keys
//! by raw atom index, while a **compound** key's atom elements are compared recursively
//! by `beam_term::compare::cmp` → `compare::cmp_atoms`, which orders two *named* atoms by
//! **name** — the C's `erts_cmp_atoms`
//! (`erts/emulator/beam/erl_utils.h:193-220`: an `ord0` fast path, then `sys_memcmp` on
//! the name, then the length). A fresh or empty table would leave every name unknown
//! there and fall back to the raw atom index, an order the C never produces.
//!
//! The seeded table below deliberately puts `none` at index 4, ahead of `a` (5) and `b`
//! (6), while `none` sorts **after** both by name — so a compound key's atom element is
//! exactly where the two rules would disagree. Because both writers now share the one
//! namespace, `maps:from_list/1` and `maps:put/3` store such a key in the same position:
//! [`a_compound_key_whose_atom_element_sorts_differently_matches_the_rebuild`] pins the
//! `maps:to_list/1` order and the `term_to_binary/1` (`MAP_EXT`) image of the two
//! routes for that key.
//!
//! # Oracle cross-check
//!
//! `tests/maps_hamt.rs` and `tests/maps_order.rs` already pin the two representations'
//! stored orders and the live oracle's pair order; this file does **not** re-derive
//! them. The one live cross-check here is
//! [`path_copied_results_match_the_live_oracle_term_to_binary_images`], which compiles a
//! probe module with the system `erlc` and compares the `MAP_EXT` image of every
//! writer's result byte for byte (skipping LOUDLY when the toolchain is unavailable).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{bitstring_bytes, cons_head, cons_tail, make_binary, tuple_arity};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::map::{
    map_put_cow_words_required, map_remove_cow_words_required, map_words_capacity,
    MAP_SMALL_MAP_LIMIT,
};
use beam_term::{
    compare, ihash::erts_map_hash, is_nil, make_small, signed_value, AtomTable, Eterm,
};

/// The atoms these cases name (all pre-seeded: no `AtomInterner` seam is installed, so
/// an unseeded name would not resolve). The order is deliberately **not** alphabetical:
/// `none` is seeded ahead of `a` and `b`, so an atom's index order and its name order
/// disagree — the one fixture in which the module docs' *One atom namespace* rule is
/// observable at all (a table whose two orders agree would hide it).
const ATOMS: [&str; 12] = [
    "true", "false", "ok", "error", "none", "a", "b", "zz", "badmap", "badkey", "x", "y",
];

/// The pair count of the large-map matrix: far above `MAP_SMALL_MAP_LIMIT`, so the trie
/// has several levels and a path copy rewrites a handful of nodes out of thousands.
const LARGE_KEYS: i64 = 2500;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(ATOMS);
    atoms
}

/// Runs `f` with the real [`default_registry`] over a fresh heap of `words` words and
/// the seeded atom table — no optional seams installed (none of these BIFs needs one).
fn run_sized<R>(words: usize, f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let reg = default_registry();
    let mut heap = Heap::new(words).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx)
}

/// [`run_sized`] with the default test heap.
fn run<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    run_sized(1 << 18, f)
}

fn invoke(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Result<Eterm> {
    reg.call(module, function, args.len() as u32, ctx, args)
}

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

fn int(v: i64) -> Eterm {
    make_small(v)
}

fn atom(ctx: &BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name)
        .unwrap_or_else(|_| panic!("the atom `{name}` is seeded"))
}

fn is_atom_named(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    beam_term::is_atom(term) && ctx.atoms().name(beam_term::atom_val(term)) == Some(name)
}

/// `maps:new/0` — a **fresh** empty map term on the test heap.
fn empty(reg: &BifRegistry, ctx: &mut BifContext<'_>) -> Eterm {
    call(reg, ctx, "maps", "new", &[])
}

/// `maps:from_list/1` over already-built terms — the **retained from-scratch builder**
/// (`write_map`), which is this suite's reference implementation.
fn map_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let mut tuples = Vec::with_capacity(pairs.len());
    for &(k, v) in pairs {
        tuples.push(ctx.tuple(&[k, v]).expect("a 2-tuple fits"));
    }
    let list = ctx.list(&tuples).expect("the pair list fits");
    call(reg, ctx, "maps", "from_list", &[list])
}

/// `maps:from_list/1` over small-integer pairs.
fn map_of_ints(reg: &BifRegistry, ctx: &mut BifContext<'_>, pairs: &[(i64, i64)]) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = pairs.iter().map(|&(k, v)| (int(k), int(v))).collect();
    map_of(reg, ctx, &pairs)
}

fn size_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> i64 {
    signed_value(call(reg, ctx, "maps", "size", &[map]))
}

/// The value `key` is bound to (`maps:get/2`, exact-key).
fn get(reg: &BifRegistry, ctx: &mut BifContext<'_>, key: Eterm, map: Eterm) -> Eterm {
    call(reg, ctx, "maps", "get", &[key, map])
}

fn is_key(reg: &BifRegistry, ctx: &mut BifContext<'_>, key: Eterm, map: Eterm) -> bool {
    let got = call(reg, ctx, "maps", "is_key", &[key, map]);
    assert!(
        is_atom_named(ctx, got, "true") || is_atom_named(ctx, got, "false"),
        "maps:is_key/2 must answer a boolean, got {got:?}"
    );
    is_atom_named(ctx, got, "true")
}

/// The `{Key, Value}` pairs of `map` in its **stored** order, read through
/// `maps:to_list/1` and the term accessors — the order-defining observable.
fn pairs_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Vec<(Eterm, Eterm)> {
    let list = call(reg, ctx, "maps", "to_list", &[map]);
    let mut out = Vec::new();
    let mut cursor = list;
    while !is_nil(cursor) {
        let head = cons_head(cursor).expect("maps:to_list/1 answers a proper list");
        assert_eq!(
            tuple_arity(head).expect("every element is a 2-tuple"),
            2,
            "maps:to_list/1 answers `{{Key, Value}}`"
        );
        out.push((
            beam_codec::term::tuple_element(head, 0).expect("a key"),
            beam_codec::term::tuple_element(head, 1).expect("a value"),
        ));
        cursor = cons_tail(cursor).expect("maps:to_list/1 answers a proper list");
    }
    out
}

/// The keys of `map` in its stored order.
fn keys_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Vec<Eterm> {
    pairs_of(reg, ctx, map)
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

/// The `erlang:term_to_binary/1` image of `term` — the third observable, and the one a
/// stored-order mistake cannot hide from.
fn image_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> Vec<u8> {
    let image = call(reg, ctx, "erlang", "term_to_binary", &[term]);
    let (bytes, offset, size_bits) = bitstring_bytes(image).expect("a binary image");
    assert_eq!(offset, 0, "term_to_binary/1 is whole-byte aligned");
    assert_eq!(size_bits % 8, 0, "term_to_binary/1 is whole-byte aligned");
    bytes[..size_bits / 8].to_vec()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The one predicate the identity arms are about: the two terms are **the same word**,
/// never merely `=:=` (ERTS decides those arms on the encoded word —
/// `erl_map.c`'s `found_key: if (*vs == value)`), which is what `erts_debug:same/2`
/// observes.
fn same_word(a: Eterm, b: Eterm) -> bool {
    a == b
}

/// `pairs` with `key` bound to `value` (replacing or inserting) — the from-scratch
/// builder's input for the expected result.
///
/// Keys are matched by **word**: every case below reuses the key terms it built, so a
/// key is the same word on both sides of the comparison.
fn with_put(pairs: &[(Eterm, Eterm)], key: Eterm, value: Eterm) -> Vec<(Eterm, Eterm)> {
    let mut out = pairs.to_vec();
    match out.iter_mut().find(|(k, _)| *k == key) {
        Some(pair) => pair.1 = value,
        None => out.push((key, value)),
    }
    out
}

/// `pairs` without `key` — the from-scratch builder's input for a removal's result.
fn with_remove(pairs: &[(Eterm, Eterm)], key: Eterm) -> Vec<(Eterm, Eterm)> {
    pairs.iter().copied().filter(|(k, _)| *k != key).collect()
}

/// Asserts a path-copied `result` is **indistinguishable** from a `maps:from_list/1`
/// rebuild of `expected_pairs`: same content (`=:=`), same `maps:to_list/1` order, and
/// the same `MAP_EXT` image.
///
/// The rebuild is the reference the optimisation replaced, so the three observables
/// together are the strongest statement the BIF surface can make about a map term.
#[track_caller]
fn assert_matches_rebuild(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    result: Eterm,
    expected_pairs: &[(Eterm, Eterm)],
    label: &str,
) {
    let reference = map_of(reg, ctx, expected_pairs);
    assert!(
        compare::eq(result, reference, ctx.atoms()),
        "`{label}`: the path-copied result is not `=:=` to the rebuild"
    );

    let got_pairs = pairs_of(reg, ctx, result);
    let want_pairs = pairs_of(reg, ctx, reference);
    assert_eq!(
        got_pairs.len(),
        want_pairs.len(),
        "`{label}`: maps:to_list/1 pair counts"
    );
    for (index, (got, want)) in got_pairs.iter().zip(want_pairs.iter()).enumerate() {
        assert!(
            compare::eq(got.0, want.0, ctx.atoms()),
            "`{label}`: maps:to_list/1 key #{index} differs (order included)"
        );
        assert!(
            compare::eq(got.1, want.1, ctx.atoms()),
            "`{label}`: maps:to_list/1 value #{index} differs"
        );
    }

    assert_eq!(
        image_of(reg, ctx, result),
        image_of(reg, ctx, reference),
        "`{label}`: the term_to_binary/1 (MAP_EXT) image differs from the rebuild's"
    );
}

/// Asserts `result` is the oracle's structured `error:{Tag, Value}` raise.
#[track_caller]
fn assert_reason(result: Result<Eterm>, ctx: &mut BifContext<'_>, tag: &str, value: Eterm) {
    let tag_atom = atom(ctx, tag);
    let expected = ctx.tuple(&[tag_atom, value]).expect("a 2-tuple fits");
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, beam_common::ExceptionClass::Error, "class");
            assert_eq!(raise.reason, beam_common::ExitReason::Structured, "reason");
            let live = *raise
                .live_reason()
                .expect("a structured reason carries its live term")
                .downcast_ref::<Eterm>()
                .expect("the payload is the live reason term");
            assert!(
                compare::eq(live, expected, ctx.atoms()),
                "the live reason is not `{tag}`-tagged with {value:?} (got {live:?})"
            );
        }
        other => panic!("expected a structured raise, got {other:?}"),
    }
}

/// The live reason term of a structured raise (used to encode it for the oracle
/// comparison).
#[track_caller]
fn reason_term(result: Result<Eterm>) -> Eterm {
    match result {
        Err(BeamError::Raise(raise)) => *raise
            .live_reason()
            .expect("a structured reason carries its live term")
            .downcast_ref::<Eterm>()
            .expect("the payload is the live reason term"),
        other => panic!("expected a structured raise, got {other:?}"),
    }
}

/// `{Value, Rest}` of a successful `maps:take/2`.
#[track_caller]
fn unpack_take(taken: Eterm) -> (Eterm, Eterm) {
    assert_eq!(
        tuple_arity(taken).expect("maps:take/2 answers a 2-tuple"),
        2
    );
    (
        beam_codec::term::tuple_element(taken, 0).expect("the value"),
        beam_codec::term::tuple_element(taken, 1).expect("the rest"),
    )
}

// ---------------------------------------------------------------------------
// The write matrix at one pair count (the flat/HAMT straddle: 31, 32, 33, 34)
// ---------------------------------------------------------------------------

/// Walks the whole writer matrix at `n` distinct small-integer keys (`1..=n`, each
/// bound to `key * 10`): replace, insert, the same-value-word identity arm, remove
/// present/absent, take present/absent, update present and update absent.
///
/// Every write result is additionally compared with a `maps:from_list/1` rebuild
/// through [`assert_matches_rebuild`], so the path copy cannot quietly change the
/// stored order, the representation or a single byte of the `MAP_EXT` image.
#[track_caller]
fn check_write_matrix(reg: &BifRegistry, ctx: &mut BifContext<'_>, n: i64) {
    let base: Vec<(Eterm, Eterm)> = (1..=n).map(|k| (int(k), int(k * 10))).collect();
    let m = map_of(reg, ctx, &base);
    assert_eq!(size_of(reg, ctx, m), n, "the {n}-key fixture's size");
    let (present, absent) = (int(1), int(-7));
    let stored_value = int(10);
    let fresh_value = int(9001);

    // `put` of an existing key: replace the value, size unchanged.
    let replaced = call(reg, ctx, "maps", "put", &[present, fresh_value, m]);
    assert!(
        !same_word(replaced, m),
        "{n}: a new value word must produce a new map"
    );
    assert_eq!(size_of(reg, ctx, replaced), n, "{n}: put/replace size");
    assert!(
        same_word(get(reg, ctx, present, replaced), fresh_value),
        "{n}: put/replace value"
    );
    assert_matches_rebuild(
        reg,
        ctx,
        replaced,
        &with_put(&base, present, fresh_value),
        &format!("n{n}_put_existing"),
    );

    // `put` of an absent key: insert, size + 1.
    let grown = call(reg, ctx, "maps", "put", &[absent, fresh_value, m]);
    assert_eq!(size_of(reg, ctx, grown), n + 1, "{n}: put/insert size");
    assert!(
        same_word(get(reg, ctx, absent, grown), fresh_value),
        "{n}: put/insert value"
    );
    assert_matches_rebuild(
        reg,
        ctx,
        grown,
        &with_put(&base, absent, fresh_value),
        &format!("n{n}_put_absent"),
    );

    // `put` of the stored value *word*: the input map itself (the identity arm).
    let echo = call(reg, ctx, "maps", "put", &[present, stored_value, m]);
    assert!(
        same_word(echo, m),
        "{n}: put with the same value word is the identity arm"
    );
    assert_eq!(size_of(reg, ctx, echo), n, "{n}: put/identity size");

    // `remove` of a present key: size - 1, and the key is gone.
    let shrunk = call(reg, ctx, "maps", "remove", &[present, m]);
    assert_eq!(size_of(reg, ctx, shrunk), n - 1, "{n}: remove/present size");
    assert!(
        !is_key(reg, ctx, present, shrunk),
        "{n}: remove/present key"
    );
    assert_matches_rebuild(
        reg,
        ctx,
        shrunk,
        &with_remove(&base, present),
        &format!("n{n}_remove_present"),
    );

    // `remove` of an absent key: the input map itself.
    let untouched = call(reg, ctx, "maps", "remove", &[absent, m]);
    assert!(
        same_word(untouched, m),
        "{n}: remove of an absent key is the identity arm"
    );

    // `take` of a present key: `{Value, Rest}`, the stored value word.
    let (value, rest) = unpack_take(call(reg, ctx, "maps", "take", &[present, m]));
    assert!(
        same_word(value, stored_value),
        "{n}: maps:take/2 hands back the stored value word"
    );
    assert_eq!(size_of(reg, ctx, rest), n - 1, "{n}: take/present size");
    assert!(!is_key(reg, ctx, present, rest), "{n}: take/present key");
    assert_matches_rebuild(
        reg,
        ctx,
        rest,
        &with_remove(&base, present),
        &format!("n{n}_take_present"),
    );

    // `take` of an absent key: the atom `error`.
    let miss = call(reg, ctx, "maps", "take", &[absent, m]);
    assert!(
        is_atom_named(ctx, miss, "error"),
        "{n}: maps:take/2 of an absent key is `error`"
    );

    // `update` of a present key: replace, size unchanged.
    let updated = call(reg, ctx, "maps", "update", &[present, fresh_value, m]);
    assert_eq!(size_of(reg, ctx, updated), n, "{n}: update/present size");
    assert!(
        same_word(get(reg, ctx, present, updated), fresh_value),
        "{n}: update/present value"
    );
    assert_matches_rebuild(
        reg,
        ctx,
        updated,
        &with_put(&base, present, fresh_value),
        &format!("n{n}_update_present"),
    );

    // `update` of an absent key: `error:{badkey, Key}`, never an insert.
    let bad = invoke(reg, ctx, "maps", "update", &[absent, fresh_value, m]);
    assert_reason(bad, ctx, "badkey", absent);
}

#[test]
fn write_matrix_at_31_pairs() {
    run(|reg, ctx| check_write_matrix(reg, ctx, 31));
}

/// At exactly `MAP_SMALL_MAP_LIMIT` the map is still flat, so a `put` that inserts the
/// 33rd key is the conversion case.
#[test]
fn write_matrix_at_32_pairs() {
    assert_eq!(
        MAP_SMALL_MAP_LIMIT, 32,
        "the flat-map limit this suite straddles"
    );
    run(|reg, ctx| check_write_matrix(reg, ctx, 32));
}

/// At 33 pairs the fixture is already a HAMT, so every arm is a trie path copy.
#[test]
fn write_matrix_at_33_pairs() {
    run(|reg, ctx| check_write_matrix(reg, ctx, 33));
}

#[test]
fn write_matrix_at_34_pairs() {
    run(|reg, ctx| check_write_matrix(reg, ctx, 34));
}

/// The representation follows the **distinct key count**, and the path-copying writers
/// honour both conversions: a `put` of a 33rd key turns the flat map into a HAMT, and a
/// `remove`/`take` back to 32 pairs turns it into a flat map again.
///
/// The observable is `maps:to_list/1`'s order: a flat map's integer keys come out in
/// ascending exact-term order, a HAMT's in trie slot order (which for 33 integer keys is
/// not ascending — the exact permutation is pinned by `tests/maps_hamt.rs`).
#[test]
fn a_put_at_the_limit_converts_flat_to_hamt_and_remove_converts_back() {
    run(|reg, ctx| {
        let base: Vec<(Eterm, Eterm)> = (1..=32).map(|k| (int(k), int(k * 10))).collect();
        let flat = map_of(reg, ctx, &base);
        let flat_keys = keys_of(reg, ctx, flat);
        let ascending: Vec<Eterm> = (1..=32).map(int).collect();
        assert_eq!(flat_keys, ascending, "a 32-key flat map's ascending order");

        // 32 -> 33: the insert converts the representation.
        let grown = call(reg, ctx, "maps", "put", &[int(33), int(330), flat]);
        assert_eq!(size_of(reg, ctx, grown), 33, "32 -> 33 size");
        let grown_keys = keys_of(reg, ctx, grown);
        let reference = map_of_ints(reg, ctx, &(1..=33).map(|k| (k, k * 10)).collect::<Vec<_>>());
        assert_eq!(
            grown_keys,
            keys_of(reg, ctx, reference),
            "the grown map is in the from_list-built HAMT's slot order"
        );
        assert_ne!(
            grown_keys
                .iter()
                .map(|&k| signed_value(k))
                .collect::<Vec<_>>(),
            (1..=33).collect::<Vec<i64>>(),
            "33 keys are a HAMT, so the order is the trie's, not ascending"
        );

        // 33 -> 32 again, through `remove`: back to a flat map, ascending order.
        let shrunk = call(reg, ctx, "maps", "remove", &[int(33), grown]);
        assert_eq!(size_of(reg, ctx, shrunk), 32, "33 -> 32 size");
        assert_eq!(
            keys_of(reg, ctx, shrunk),
            ascending,
            "a remove back to 32 pairs converts to a flat map again"
        );
        assert_matches_rebuild(reg, ctx, shrunk, &base, "hamt_to_flat_remove");

        // 33 -> 32 again, through `take`: the same conversion, with the pair returned.
        let (value, rest) = unpack_take(call(reg, ctx, "maps", "take", &[int(33), grown]));
        assert!(same_word(value, int(330)), "the taken value");
        assert_eq!(size_of(reg, ctx, rest), 32, "take 33 -> 32 size");
        assert_eq!(
            keys_of(reg, ctx, rest),
            ascending,
            "a take back to 32 pairs converts to a flat map again"
        );
        assert_matches_rebuild(reg, ctx, rest, &base, "hamt_to_flat_take");

        // Adding a 33rd key to the flat map a second time reproduces the same HAMT.
        let regrown = call(reg, ctx, "maps", "put", &[int(33), int(330), shrunk]);
        assert_eq!(
            image_of(reg, ctx, regrown),
            image_of(reg, ctx, grown),
            "converting twice produces the same image"
        );
    });
}

// ---------------------------------------------------------------------------
// A large map (2 500 keys)
// ---------------------------------------------------------------------------

/// The same matrix on a 2 500-key HAMT: a representative subset (insert, replace,
/// remove present/absent, take present/absent) at a size where a full rebuild would be
/// thousands of times the work of a path copy.
///
/// The fixture is built **once** and every arm derives from it, so the test stays fast;
/// the two arms that change the size are compared with a rebuild, the two identity arms
/// with the input map word.
#[test]
fn write_matrix_at_2500_keys() {
    run_sized(1 << 21, |reg, ctx| {
        let base: Vec<(Eterm, Eterm)> = (1..=LARGE_KEYS).map(|k| (int(k), int(k * 10))).collect();
        let m = map_of(reg, ctx, &base);
        assert_eq!(size_of(reg, ctx, m), LARGE_KEYS, "the fixture's size");
        let (present, absent) = (int(1), int(LARGE_KEYS + 1));
        let stored_value = int(10);
        let fresh_value = int(9001);

        // Insert an absent key.
        let grown = call(reg, ctx, "maps", "put", &[absent, fresh_value, m]);
        assert_eq!(size_of(reg, ctx, grown), LARGE_KEYS + 1, "put/insert size");
        assert!(same_word(get(reg, ctx, absent, grown), fresh_value));
        assert_matches_rebuild(
            reg,
            ctx,
            grown,
            &with_put(&base, absent, fresh_value),
            "large_put_absent",
        );

        // Replace a present key's value.
        let replaced = call(reg, ctx, "maps", "put", &[present, fresh_value, m]);
        assert_eq!(size_of(reg, ctx, replaced), LARGE_KEYS, "put/replace size");
        assert!(
            same_word(get(reg, ctx, present, replaced), fresh_value),
            "put/replace value"
        );
        assert_matches_rebuild(
            reg,
            ctx,
            replaced,
            &with_put(&base, present, fresh_value),
            "large_put_existing",
        );

        // The identity arms survive at 2 500 pairs too.
        let echo = call(reg, ctx, "maps", "put", &[present, stored_value, m]);
        assert!(same_word(echo, m), "put with the same value word");
        let untouched = call(reg, ctx, "maps", "remove", &[absent, m]);
        assert!(same_word(untouched, m), "remove of an absent key");

        // Remove a present key.
        let shrunk = call(reg, ctx, "maps", "remove", &[present, m]);
        assert_eq!(size_of(reg, ctx, shrunk), LARGE_KEYS - 1, "remove size");
        assert!(!is_key(reg, ctx, present, shrunk), "the key is gone");
        assert_matches_rebuild(
            reg,
            ctx,
            shrunk,
            &with_remove(&base, present),
            "large_remove_present",
        );

        // `take`: present answers `{Value, Rest}`, absent the atom `error`.
        let (value, rest) = unpack_take(call(reg, ctx, "maps", "take", &[present, m]));
        assert!(same_word(value, stored_value), "take hands back the value");
        assert_eq!(size_of(reg, ctx, rest), LARGE_KEYS - 1, "take size");
        assert_matches_rebuild(
            reg,
            ctx,
            rest,
            &with_remove(&base, present),
            "large_take_present",
        );
        let miss = call(reg, ctx, "maps", "take", &[absent, m]);
        assert!(is_atom_named(ctx, miss, "error"), "take of an absent key");

        // An absent `update` is still `error:{badkey, Key}` at this size.
        let bad = invoke(reg, ctx, "maps", "update", &[absent, fresh_value, m]);
        assert_reason(bad, ctx, "badkey", absent);
    });
}

// ---------------------------------------------------------------------------
// Mixed and compound keys
// ---------------------------------------------------------------------------

/// `1` and `1.0` are **distinct** keys (`=:=` identity), so a path copy that keys on
/// the small-integer band must keep both — in either direction.
#[test]
fn one_and_one_point_zero_stay_distinct_through_a_path_copy() {
    run(|reg, ctx| {
        let one = int(1);
        let float_one = ctx.make_float(1.0).expect("a float term fits");
        let float_two = ctx.make_float(2.0).expect("a float term fits");
        let x = atom(ctx, "x");
        let y = atom(ctx, "y");

        // `maps:put(1, x, #{1.0 => y})` keeps both keys.
        let float_keyed = map_of(reg, ctx, &[(float_one, y)]);
        assert_eq!(size_of(reg, ctx, float_keyed), 1, "#{{1.0 => y}}");
        let both = call(reg, ctx, "maps", "put", &[one, x, float_keyed]);
        assert_eq!(size_of(reg, ctx, both), 2, "the integer key is inserted");
        assert!(same_word(get(reg, ctx, one, both), x), "1 => x");
        assert!(same_word(get(reg, ctx, float_one, both), y), "1.0 => y");
        assert_matches_rebuild(
            reg,
            ctx,
            both,
            &[(float_one, y), (one, x)],
            "mixed_put_int_over_float",
        );

        // The replacement arm for the integer key must not touch the float key.
        let replaced = call(reg, ctx, "maps", "put", &[one, y, both]);
        assert_eq!(
            size_of(reg, ctx, replaced),
            2,
            "put replaces, never inserts"
        );
        assert!(same_word(get(reg, ctx, one, replaced), y), "1 => y");
        assert!(same_word(get(reg, ctx, float_one, replaced), y), "1.0 => y");
        assert_matches_rebuild(
            reg,
            ctx,
            replaced,
            &[(one, y), (float_one, y)],
            "mixed_put_int_replace",
        );

        // `remove(1.0, …)` leaves the integer key; `take(1, …)` leaves the float key.
        let without_float = call(reg, ctx, "maps", "remove", &[float_one, both]);
        assert_eq!(size_of(reg, ctx, without_float), 1, "remove 1.0");
        assert!(is_key(reg, ctx, one, without_float), "1 survives");
        assert!(!is_key(reg, ctx, float_one, without_float), "1.0 is gone");

        let (value, rest) = unpack_take(call(reg, ctx, "maps", "take", &[one, both]));
        assert!(same_word(value, x), "take 1 => x");
        assert_eq!(size_of(reg, ctx, rest), 1, "take 1");
        let float_only = map_of(reg, ctx, &[(float_one, y)]);
        assert_eq!(
            image_of(reg, ctx, rest),
            image_of(reg, ctx, float_only),
            "the rest is exactly #{{1.0 => y}}"
        );

        // The reverse direction: an integer-keyed map accepts the float key.
        let int_keyed = map_of(reg, ctx, &[(one, x)]);
        let both = call(reg, ctx, "maps", "put", &[float_one, y, int_keyed]);
        assert_eq!(size_of(reg, ctx, both), 2, "#{{1 => x}} plus 1.0 => y");
        assert!(same_word(get(reg, ctx, one, both), x), "1 => x");
        assert!(same_word(get(reg, ctx, float_one, both), y), "1.0 => y");
        assert_matches_rebuild(
            reg,
            ctx,
            both,
            &[(one, x), (float_one, y)],
            "mixed_put_float_over_int",
        );

        // The same distinction holds above the flat limit (a HAMT hashes `1` and `1.0`
        // differently, so they cannot share a cell).
        let mut hamt_pairs: Vec<(Eterm, Eterm)> = (1..=32).map(|k| (int(k), int(k))).collect();
        hamt_pairs.push((float_one, int(0)));
        let hamt = map_of(reg, ctx, &hamt_pairs);
        assert_eq!(size_of(reg, ctx, hamt), 33, "32 ints + 1.0 is a HAMT");
        let with_two_float = call(reg, ctx, "maps", "put", &[float_two, int(0), hamt]);
        assert_eq!(size_of(reg, ctx, with_two_float), 34, "2.0 is a 34th key");
        let without_int_one = call(reg, ctx, "maps", "remove", &[one, with_two_float]);
        assert_eq!(size_of(reg, ctx, without_int_one), 33, "1 removed");
        assert!(is_key(reg, ctx, float_one, without_int_one), "1.0 survives");
        assert!(is_key(reg, ctx, float_two, without_int_one), "2.0 survives");
    });
}

/// Binary keys and compound (`{Atom, Integer}`) keys survive a path copy, and a flat
/// map's `maps:to_list/1` order agrees with a `maps:from_list/1` build over the same
/// pairs — including for the compound keys, whose order is decided by
/// `erts_cmp_flatmap_keys` (atom elements by raw atom index).
#[test]
fn binary_and_compound_keys_survive_a_path_copy_in_the_from_list_order() {
    run(|reg, ctx| {
        let (a, b, x) = (atom(ctx, "a"), atom(ctx, "b"), atom(ctx, "x"));
        let one = int(1);
        let two = int(2);
        let bin1 = make_binary(ctx.heap(), &[1]).expect("a 1-byte binary fits");
        let bin2 = make_binary(ctx.heap(), &[1, 2]).expect("a 2-byte binary fits");
        let tuple_a1 = ctx.tuple(&[a, one]).expect("a 2-tuple fits");
        let tuple_b2 = ctx.tuple(&[b, two]).expect("a 2-tuple fits");

        let base = [
            (bin2, int(20)),
            (tuple_b2, int(22)),
            (bin1, int(10)),
            (tuple_a1, int(21)),
            (x, int(1)),
        ];
        let m = map_of(reg, ctx, &base);
        assert_eq!(size_of(reg, ctx, m), 5, "five distinct keys");

        // The stored order of the from_list-built map is the reference; a path copy that
        // inserted a key without honouring `erts_cmp_flatmap_keys` would land elsewhere.
        let reference_order = keys_of(reg, ctx, m);
        assert_eq!(reference_order.len(), 5, "the flat map stores five keys");

        // Replace a compound key's value: the order must not move.
        let replaced = call(reg, ctx, "maps", "put", &[tuple_a1, int(99), m]);
        assert_eq!(
            keys_of(reg, ctx, replaced),
            reference_order,
            "replacing a compound key's value must not reorder the keys"
        );
        assert_matches_rebuild(
            reg,
            ctx,
            replaced,
            &with_put(&base, tuple_a1, int(99)),
            "compound_put_replace",
        );

        // Insert a new compound key: the order is the builder's, not the insertion's.
        //
        // The inserted key's atom element is chosen so that its **index** order and its
        // **name** order agree with the two keys already stored (`b` sorts after `a`
        // either way, and `{b,3}` after `{b,2}` by the integer); the case where they
        // disagree is the module docs' *One atom namespace* section.
        let tuple_b3 = ctx.tuple(&[b, int(3)]).expect("a 2-tuple fits");
        let grown = call(reg, ctx, "maps", "put", &[tuple_b3, int(23), m]);
        assert_eq!(size_of(reg, ctx, grown), 6, "six distinct keys");
        assert_matches_rebuild(
            reg,
            ctx,
            grown,
            &with_put(&base, tuple_b3, int(23)),
            "compound_put_insert",
        );

        // Binary keys: exact bytes, so `<<1>>` and `<<1,2>>` are two keys.
        assert!(is_key(reg, ctx, bin1, m) && is_key(reg, ctx, bin2, m));
        let without_bin1 = call(reg, ctx, "maps", "remove", &[bin1, m]);
        assert_eq!(size_of(reg, ctx, without_bin1), 4, "remove <<1>>");
        assert!(!is_key(reg, ctx, bin1, without_bin1), "<<1>> is gone");
        assert!(is_key(reg, ctx, bin2, without_bin1), "<<1,2>> survives");
        assert_matches_rebuild(
            reg,
            ctx,
            without_bin1,
            &with_remove(&base, bin1),
            "binary_remove_present",
        );

        let (value, rest) = unpack_take(call(reg, ctx, "maps", "take", &[tuple_b2, m]));
        assert!(same_word(value, int(22)), "the compound key's value");
        assert_eq!(size_of(reg, ctx, rest), 4, "take {{b,2}}");
        assert_matches_rebuild(
            reg,
            ctx,
            rest,
            &with_remove(&base, tuple_b2),
            "compound_take_present",
        );

        // A binary key that is absent changes nothing.
        let absent = make_binary(ctx.heap(), &[9]).expect("a 1-byte binary fits");
        let untouched = call(reg, ctx, "maps", "remove", &[absent, m]);
        assert!(same_word(untouched, m), "remove of an absent binary key");
    });
}

/// The **order** half of the one-namespace rule the module docs describe: an atom
/// element whose **index** order and **name** order disagree — `none` is seeded at
/// index 4, ahead of `a` (5) and `b` (6), but sorts after both by name — is stored in
/// the **same** position by `maps:from_list/1` and by `maps:put/3`.
///
/// `erts_cmp_flatmap_keys` keeps the two *atom* keys in raw-index order and orders a
/// compound key's atom element by **name** (`compare::cmp_atoms`, the C's
/// `erts_cmp_atoms`), so `{a,1} < {b,2} < {none,3}` — the opposite of what an empty
/// atom table would give (`{none,3}` first). Both writers are handed the same live
/// `ctx.atoms()`, so the rebuild and the path copy agree on the order, on the content
/// and on the `MAP_EXT` bytes.
#[test]
fn a_compound_key_whose_atom_element_sorts_differently_matches_the_rebuild() {
    run(|reg, ctx| {
        // The fixture's premise: index order and name order really do disagree.
        let (none_atom, a_atom) = (
            ctx.atoms().lookup("none").expect("`none` is seeded"),
            ctx.atoms().lookup("a").expect("`a` is seeded"),
        );
        assert!(
            none_atom.raw() < a_atom.raw(),
            "the seeded table must put `none` at a lower index than `a`"
        );

        let (a, b, none) = (atom(ctx, "a"), atom(ctx, "b"), atom(ctx, "none"));
        let tuple_a1 = ctx.tuple(&[a, int(1)]).expect("a 2-tuple fits");
        let tuple_b2 = ctx.tuple(&[b, int(2)]).expect("a 2-tuple fits");
        let tuple_n3 = ctx.tuple(&[none, int(3)]).expect("a 2-tuple fits");

        let base = [(tuple_a1, int(10)), (tuple_b2, int(20))];
        let m = map_of(reg, ctx, &base);
        let grown = call(reg, ctx, "maps", "put", &[tuple_n3, int(30), m]);
        let reference = map_of(reg, ctx, &with_put(&base, tuple_n3, int(30)));

        assert_eq!(size_of(reg, ctx, grown), 3, "three distinct keys");
        assert!(
            compare::eq(grown, reference, ctx.atoms()),
            "the path-copied map is `=:=` to the rebuild"
        );
        assert!(
            same_word(get(reg, ctx, tuple_a1, grown), int(10)),
            "{{a,1}}"
        );
        assert!(
            same_word(get(reg, ctx, tuple_b2, grown), int(20)),
            "{{b,2}}"
        );
        assert!(
            same_word(get(reg, ctx, tuple_n3, grown), int(30)),
            "{{none,3}}"
        );
        for (key, _) in pairs_of(reg, ctx, reference) {
            assert!(
                is_key(reg, ctx, key, grown),
                "every key of the rebuild is a key of the path-copied map"
            );
        }

        // The order: the name order `erts_cmp_atoms` implies, on **both** routes.
        let want_keys = vec![tuple_a1, tuple_b2, tuple_n3];
        let got_keys = keys_of(reg, ctx, grown);
        assert_eq!(got_keys.len(), want_keys.len(), "three stored keys");
        for (index, (got, want)) in got_keys.iter().zip(want_keys.iter()).enumerate() {
            assert!(
                same_word(*got, *want),
                "maps:put/3 stores key #{index} in the compound key's name order \
                 (`none` sorts last by name, first by index)"
            );
        }
        let reference_keys = keys_of(reg, ctx, reference);
        for (index, (got, want)) in reference_keys.iter().zip(want_keys.iter()).enumerate() {
            assert!(
                same_word(*got, *want),
                "maps:from_list/1 stores key #{index} in the same order"
            );
        }

        // ... and the encoded image, the observable a stored-order mistake cannot hide
        // from.
        assert_eq!(
            image_of(reg, ctx, grown),
            image_of(reg, ctx, reference),
            "the two writers' MAP_EXT images agree byte for byte (order included)"
        );

        // The removal arms agree with each other and with the rebuild's content.
        let shrunk = call(reg, ctx, "maps", "remove", &[tuple_n3, grown]);
        assert_eq!(size_of(reg, ctx, shrunk), 2, "remove {{none,3}}");
        assert!(!is_key(reg, ctx, tuple_n3, shrunk));
        assert!(
            compare::eq(shrunk, m, ctx.atoms()),
            "removing the inserted key restores the original content"
        );
        assert_matches_rebuild(reg, ctx, shrunk, &base, "compound_none_remove");
        let (value, rest) = unpack_take(call(reg, ctx, "maps", "take", &[tuple_n3, grown]));
        assert!(same_word(value, int(30)), "take hands back the value");
        assert!(
            !is_key(reg, ctx, tuple_n3, rest),
            "take's rest no longer holds the key"
        );
        assert!(
            compare::eq(rest, shrunk, ctx.atoms()),
            "take's rest and remove agree"
        );
        assert_matches_rebuild(reg, ctx, rest, &base, "compound_none_take");
    });
}

// ---------------------------------------------------------------------------
// Keys that share low hash nibbles
// ---------------------------------------------------------------------------

/// Brute-forces `count` small-integer keys whose `erts_map_hash` agrees on its low 8
/// bits — the first **two** trie levels (`hashmap_index` consumes
/// `(hash >> 4*L) & 0xF`), so the group descends through the same level-0 slot and the
/// same level-1 node.
///
/// The hash of an immediate is `ihash_mix64(word)` (`erl_term_hashing.c`), so the group
/// is a property of the integer keys alone and is stable here; the search is brute force
/// exactly so no hash constant has to be copied into this file.
fn keys_sharing_low_byte(count: usize) -> Vec<i64> {
    let mut buckets: HashMap<u64, Vec<i64>> = HashMap::new();
    for key in 1..=20_000i64 {
        let low = erts_map_hash(int(key)) & 0xFF;
        let bucket = buckets.entry(low).or_default();
        bucket.push(key);
        if bucket.len() == count {
            return bucket.clone();
        }
    }
    panic!("no {count} small integers share their low 8 hash bits below 20 000");
}

/// Keys that collide on the low two nibbles still exercise distinct cells: they share
/// the level-0 slot and the level-1 node, so a path copy rewrites a subtree holding
/// **several** pairs rather than a single-pair leaf.
///
/// A genuine *collision node* — two keys with an identical **full 64-bit** hash, whose
/// cells are then compared by `=:=` — cannot be built from ordinary atoms or small
/// integers, so that interior arm is pinned by `beam_term`'s own tests
/// (`src/map/cow/tests.rs`'s `collision_node_*` family) rather than from here.
#[test]
fn keys_sharing_low_hash_nibbles_path_copy_correctly() {
    let group = keys_sharing_low_byte(4);
    for &key in &group {
        assert_eq!(
            erts_map_hash(int(key)) & 0xFF,
            erts_map_hash(int(group[0])) & 0xFF,
            "the brute-forced group must agree on its low 8 hash bits"
        );
    }

    run(|reg, ctx| {
        // The flat tier first: the group alone is four pairs, well under the limit.
        let flat_pairs: Vec<(Eterm, Eterm)> =
            group.iter().map(|&k| (int(k), int(k * 10))).collect();
        let flat = map_of(reg, ctx, &flat_pairs);
        assert_eq!(size_of(reg, ctx, flat), 4, "the group alone is flat");
        let target = int(group[2]);
        let replaced = call(reg, ctx, "maps", "put", &[target, int(-1), flat]);
        assert_eq!(size_of(reg, ctx, replaced), 4, "flat replace size");
        assert!(same_word(get(reg, ctx, target, replaced), int(-1)));
        assert_matches_rebuild(
            reg,
            ctx,
            replaced,
            &with_put(&flat_pairs, target, int(-1)),
            "nibble_flat_replace",
        );
        let shrunk = call(reg, ctx, "maps", "remove", &[target, flat]);
        assert_eq!(size_of(reg, ctx, shrunk), 3, "flat remove size");
        assert_matches_rebuild(
            reg,
            ctx,
            shrunk,
            &with_remove(&flat_pairs, target),
            "nibble_flat_remove",
        );

        // The HAMT tier: `1..=40` plus whichever group members lie beyond it, so the
        // shared node sits inside a multi-level trie.
        let mut hamt_pairs: Vec<(Eterm, Eterm)> = (1..=40).map(|k| (int(k), int(k * 10))).collect();
        for &key in &group {
            if !(1..=40).contains(&key) {
                hamt_pairs.push((int(key), int(key * 10)));
            }
        }
        let hamt_size = hamt_pairs.len() as i64;
        assert!(
            hamt_size > MAP_SMALL_MAP_LIMIT as i64,
            "the fixture must be a HAMT ({hamt_size} keys)"
        );
        let hamt = map_of(reg, ctx, &hamt_pairs);
        assert_eq!(size_of(reg, ctx, hamt), hamt_size, "the HAMT fixture");
        assert!(
            group
                .iter()
                .all(|&k| hamt_pairs.iter().any(|(key, _)| *key == int(k))),
            "the fixture must contain the whole group"
        );

        let replaced = call(reg, ctx, "maps", "put", &[target, int(-1), hamt]);
        assert_eq!(size_of(reg, ctx, replaced), hamt_size, "HAMT replace size");
        assert!(same_word(get(reg, ctx, target, replaced), int(-1)));
        assert_matches_rebuild(
            reg,
            ctx,
            replaced,
            &with_put(&hamt_pairs, target, int(-1)),
            "nibble_hamt_replace",
        );

        // An absent key — no shared node — inserts one pair.
        let absent = int(-1);
        let grown = call(reg, ctx, "maps", "put", &[absent, int(-2), hamt]);
        assert_eq!(size_of(reg, ctx, grown), hamt_size + 1, "HAMT put size");
        assert!(same_word(get(reg, ctx, absent, grown), int(-2)));
        assert_matches_rebuild(
            reg,
            ctx,
            grown,
            &with_put(&hamt_pairs, absent, int(-2)),
            "nibble_hamt_insert",
        );

        let shrunk = call(reg, ctx, "maps", "remove", &[target, hamt]);
        assert_eq!(size_of(reg, ctx, shrunk), hamt_size - 1, "HAMT remove size");
        assert_matches_rebuild(
            reg,
            ctx,
            shrunk,
            &with_remove(&hamt_pairs, target),
            "nibble_hamt_remove",
        );

        let (value, rest) = unpack_take(call(reg, ctx, "maps", "take", &[target, hamt]));
        assert!(
            same_word(value, int(group[2] * 10)),
            "the group member's stored value"
        );
        assert_eq!(size_of(reg, ctx, rest), hamt_size - 1, "HAMT take size");
        assert_matches_rebuild(
            reg,
            ctx,
            rest,
            &with_remove(&hamt_pairs, target),
            "nibble_hamt_take",
        );
    });
}

// ---------------------------------------------------------------------------
// Equivalence with the retained from-scratch builder
// ---------------------------------------------------------------------------

/// Every writer result compared with a `maps:from_list/1` rebuild over the expected
/// pairs, side by side across the whole size range — the explicit, single-place
/// statement of the equivalence the arms above assert one shape at a time.
///
/// The `term_to_binary/1` half is the strongest of the three observables: it fixes the
/// representation (flat vs HAMT), the stored order and every value byte in one image.
#[test]
fn every_path_copied_result_matches_the_from_scratch_rebuild() {
    run_sized(1 << 20, |reg, ctx| {
        for n in [1i64, 2, 31, 32, 33, 34, 40] {
            let base: Vec<(Eterm, Eterm)> = (1..=n).map(|k| (int(k), int(k * 10))).collect();
            let m = map_of(reg, ctx, &base);
            let (present, absent) = (int(n / 2 + 1), int(-7));

            // Insert, replace, remove present, take present: the four writing arms.
            let inserted = call(reg, ctx, "maps", "put", &[absent, int(1), m]);
            assert_matches_rebuild(
                reg,
                ctx,
                inserted,
                &with_put(&base, absent, int(1)),
                &format!("equivalence_n{n}_insert"),
            );
            let replaced = call(reg, ctx, "maps", "put", &[present, int(2), m]);
            assert_matches_rebuild(
                reg,
                ctx,
                replaced,
                &with_put(&base, present, int(2)),
                &format!("equivalence_n{n}_replace"),
            );
            let removed = call(reg, ctx, "maps", "remove", &[present, m]);
            assert_matches_rebuild(
                reg,
                ctx,
                removed,
                &with_remove(&base, present),
                &format!("equivalence_n{n}_remove"),
            );
            let (_, rest) = unpack_take(call(reg, ctx, "maps", "take", &[present, m]));
            assert_matches_rebuild(
                reg,
                ctx,
                rest,
                &with_remove(&base, present),
                &format!("equivalence_n{n}_take"),
            );
            let updated = call(reg, ctx, "maps", "update", &[present, int(3), m]);
            assert_matches_rebuild(
                reg,
                ctx,
                updated,
                &with_put(&base, present, int(3)),
                &format!("equivalence_n{n}_update"),
            );
        }

        // The empty map's arm: a put into `#{}` builds the one-pair flat map.
        let blank = empty(reg, ctx);
        let one_pair = call(reg, ctx, "maps", "put", &[int(1), int(10), blank]);
        assert_matches_rebuild(
            reg,
            ctx,
            one_pair,
            &[(int(1), int(10))],
            "equivalence_empty_put",
        );

        // The order the reference builder chose is reproduced even when a path copy
        // inserts a key that sorts *before* every stored one.
        let pairs: Vec<(Eterm, Eterm)> = (1..=34).map(|k| (int(k * 3), int(k))).collect();
        let m = map_of(reg, ctx, &pairs);
        let first = call(reg, ctx, "maps", "put", &[int(0), int(0), m]);
        assert_matches_rebuild(
            reg,
            ctx,
            first,
            &with_put(&pairs, int(0), int(0)),
            "equivalence_first_key_insert",
        );
    });
}

// ---------------------------------------------------------------------------
// Live oracle cross-check (compiled module, byte-exact, skips loudly)
// ---------------------------------------------------------------------------

/// The compiled probe module: for each pair count it builds the map with
/// `maps:from_list/1` and prints the hex of the `erlang:term_to_binary/1` image of every
/// writer's result, so the comparison is byte-exact and printer-free.
///
/// The maps are built at runtime and keyed by small integers only, so no atom index and
/// no literal sharing can make the oracle's images differ from this runtime's for a
/// reason that is not the writer itself.
const ORACLE_MODULE: &str = r#"
-module(map_cow_oracle).
-export([go/0]).

go() ->
    Hex = fun(T) ->
                  lists:flatten([io_lib:format("~2.16.0b", [B])
                                 || B <- binary_to_list(erlang:term_to_binary(T))])
          end,
    Rows = lists:append([size_rows(N) || N <- [31, 32, 33, 34, 40]]) ++ extra_rows(),
    [io:format("~s=~s~n", [Name, Hex(Term)]) || {Name, Term} <- Rows],
    ok.

size_rows(N) ->
    M = maps:from_list([{K, K * 10} || K <- lists:seq(1, N)]),
    P = fun(Suffix) -> "n" ++ integer_to_list(N) ++ "_" ++ Suffix end,
    [   {P("put_existing"),  maps:put(1, 9001, M)},
        {P("put_absent"),    maps:put(-7, 9001, M)},
        {P("put_same"),      maps:put(1, 10, M)},
        {P("remove_present"), maps:remove(1, M)},
        {P("remove_absent"), maps:remove(-7, M)},
        {P("take_present"),  maps:take(1, M)},
        {P("take_absent"),   maps:take(-7, M)},
        {P("update_present"), maps:update(1, 9001, M)},
        {P("update_absent"), caught(fun() -> maps:update(-7, 9001, M) end)}
    ].

extra_rows() ->
    Float = maps:from_list([{1.0, 98}]),
    Ints  = maps:from_list([{1, 98}]),
    Bin   = maps:from_list([{<<3>>, 5}, {<<1>>, 6}]),
    [   {"mixed_put_int_over_float",  maps:put(1, 99, Float)},
        {"mixed_put_float_over_int",  maps:put(1.0, 99, Ints)},
        {"mixed_remove_int_from_float", maps:remove(1, Float)},
        {"binary_put_absent",  maps:put(<<1,2>>, 7, Bin)},
        {"binary_put_present", maps:put(<<1>>, 8, Bin)},
        {"binary_take_present", maps:take(<<1>>, Bin)},
        {"binary_remove_absent", maps:remove(<<1,2>>, Bin)}
    ].

caught(F) ->
    try F() of
        Value -> Value
    catch
        error:Reason -> Reason
    end.
"#;

/// The system oracle's executable `name`, if it can be spawned: `$ERTS_ORACLE` when it
/// names an existing executable, else the program on `$PATH` — the resolution
/// `tools/differential` uses.
fn resolve_tool(name: &str) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let oracle = PathBuf::from(value);
        if let (Some(dir), Some(stem)) = (oracle.parent(), oracle.file_stem()) {
            if stem == name {
                return is_executable(&oracle).then_some(oracle);
            }
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Compiles [`ORACLE_MODULE`] with the system `erlc` and runs it with the system `erl`,
/// returning its `name=hex` rows.
///
/// `None` when the toolchain cannot be spawned at all (the caller then skips loudly); a
/// toolchain that *is* present but fails to compile or run panics, so the check can
/// never pass vacuously.
fn compiled_module_oracle() -> Option<HashMap<String, String>> {
    let erlc = resolve_tool("erlc")?;
    let erl = resolve_tool("erl")?;
    let dir = std::env::temp_dir().join("beam_bifs_maps_cow_oracle");
    std::fs::create_dir_all(&dir).ok()?;
    let source = dir.join("map_cow_oracle.erl");
    std::fs::write(&source, ORACLE_MODULE).ok()?;

    let compiled = Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    assert!(
        compiled.status.success(),
        "erlc could not compile the oracle fixture:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let run = Command::new(&erl)
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-eval")
        .arg("map_cow_oracle:go(), halt(0).")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    assert!(
        run.status.success(),
        "the oracle fixture failed to run:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let mut rows = HashMap::new();
    for line in String::from_utf8_lossy(&run.stdout).lines() {
        let Some((name, image)) = line.split_once('=') else {
            continue;
        };
        rows.insert(name.to_string(), image.trim().to_string());
    }
    Some(rows)
}

/// One oracle row: the writer's name, the arguments it is driven with, and the call.
type RowFn = fn(&BifRegistry, &mut BifContext<'_>, Eterm, Eterm, Eterm) -> Eterm;

/// This runtime's own rows, keyed exactly as [`ORACLE_MODULE`] keys them: the same
/// calls over maps built the same way (`maps:from_list/1` over small-integer pairs, so
/// no atom index and no literal sharing can differ).
/// The committed `MAP_EXT` images of every writer row (see the oracle test below).
const GOLDENS: &str = include_str!("data/maps_cow_oracle.goldens");

fn our_rows(reg: &BifRegistry, ctx: &mut BifContext<'_>) -> HashMap<String, String> {
    let mut rows = HashMap::new();
    let put: RowFn = |reg, ctx, k, v, m| call(reg, ctx, "maps", "put", &[k, v, m]);
    let update: RowFn = |reg, ctx, k, v, m| call(reg, ctx, "maps", "update", &[k, v, m]);
    let remove: RowFn = |reg, ctx, k, _v, m| call(reg, ctx, "maps", "remove", &[k, m]);
    let take: RowFn = |reg, ctx, k, _v, m| call(reg, ctx, "maps", "take", &[k, m]);

    for n in [31i64, 32, 33, 34, 40] {
        let base: Vec<(Eterm, Eterm)> = (1..=n).map(|k| (int(k), int(k * 10))).collect();
        let m = map_of(reg, ctx, &base);
        let (one, absent) = (int(1), int(-7));
        let (stored_value, fresh_value) = (int(10), int(9001));
        let prefix = format!("n{n}_");

        let specs: [(&str, Eterm, Eterm, RowFn); 8] = [
            ("put_existing", one, fresh_value, put),
            ("put_absent", absent, fresh_value, put),
            ("put_same", one, stored_value, put),
            ("remove_present", one, stored_value, remove),
            ("remove_absent", absent, stored_value, remove),
            ("take_present", one, stored_value, take),
            ("take_absent", absent, stored_value, take),
            ("update_present", one, fresh_value, update),
        ];
        for (name, key, value, writer) in specs {
            let result = writer(reg, ctx, key, value, m);
            rows.insert(format!("{prefix}{name}"), hex(&image_of(reg, ctx, result)));
        }

        // The absent-update arm's image is that of the raise's reason term
        // (`{badkey, -7}`), which is what the oracle's `catch error:Reason` prints.
        let reason = reason_term(invoke(
            reg,
            ctx,
            "maps",
            "update",
            &[absent, fresh_value, m],
        ));
        rows.insert(
            format!("{prefix}update_absent"),
            hex(&image_of(reg, ctx, reason)),
        );
    }

    // The mixed-key arms: `1` and `1.0` are distinct keys in either direction, and a
    // binary key is keyed by its exact bytes.
    let float_one = ctx.make_float(1.0).expect("a float term fits");
    let float_keyed = map_of(reg, ctx, &[(float_one, int(98))]);
    let int_keyed = map_of(reg, ctx, &[(int(1), int(98))]);
    let bin_three = make_binary(ctx.heap(), &[3]).expect("a 1-byte binary fits");
    let bin_one = make_binary(ctx.heap(), &[1]).expect("a 1-byte binary fits");
    let bin_one_two = make_binary(ctx.heap(), &[1, 2]).expect("a 2-byte binary fits");
    let binaries = map_of(reg, ctx, &[(bin_three, int(5)), (bin_one, int(6))]);

    let extra: [(&str, RowFn, Eterm, Eterm, Eterm); 7] = [
        (
            "mixed_put_int_over_float",
            put,
            int(1),
            int(99),
            float_keyed,
        ),
        (
            "mixed_put_float_over_int",
            put,
            float_one,
            int(99),
            int_keyed,
        ),
        (
            "mixed_remove_int_from_float",
            remove,
            int(1),
            int(99),
            float_keyed,
        ),
        ("binary_put_absent", put, bin_one_two, int(7), binaries),
        ("binary_put_present", put, bin_one, int(8), binaries),
        ("binary_take_present", take, bin_one, int(0), binaries),
        (
            "binary_remove_absent",
            remove,
            bin_one_two,
            int(0),
            binaries,
        ),
    ];
    for (name, writer, key, value, map) in extra {
        let result = writer(reg, ctx, key, value, map);
        rows.insert(name.to_string(), hex(&image_of(reg, ctx, result)));
    }
    rows
}

/// Committed `MAP_EXT` images for every path-copying writer result, plus the live-oracle
/// re-derivation of the same rows.
///
/// The **committed** set is the leg that always runs (CI's `cargo test` job has no
/// `erl`); the live leg then rebuilds the rows with the system `erlc`+`erl` and
/// requires the two to agree byte for byte, skipping LOUDLY when the toolchain is
/// absent. A row that only one side produced is a failure in both directions, so a
/// silently missing or extra case cannot pass.
///
/// Regenerate the golden file after an intentional change (and re-read the diff: it is
/// the oracle's answer, not ours):
/// `MAPS_COW_WRITE_GOLDENS=1 erts_rust/scripts/cargo.sh test --manifest-path
/// erts_rust/Cargo.toml -p beam_bifs --test maps_cow path_copied -- --nocapture`.
#[test]
fn path_copied_results_match_the_live_oracle_term_to_binary_images() {
    let ours = run_sized(1 << 20, |reg, ctx| our_rows(reg, ctx));
    if std::env::var_os("MAPS_COW_WRITE_GOLDENS").is_some() {
        let mut names: Vec<&String> = ours.keys().collect();
        names.sort();
        let mut text = String::from(
            "# `MAP_EXT` images of the path-copying `maps` writer results, one row per case\n\
             # (name=hex). Regenerate with MAPS_COW_WRITE_GOLDENS=1 (see tests/maps_cow.rs).\n",
        );
        for name in names {
            use std::fmt::Write as _;
            writeln!(text, "{name}={}", ours[name]).expect("formatting into a String");
        }
        let path = std::path::Path::new("tests/data/maps_cow_oracle.goldens");
        std::fs::write(path, &text).expect("write the golden file");
        eprintln!("wrote {} rows to {}", ours.len(), path.display());
        return;
    }
    let goldens = parse_goldens(GOLDENS);
    let mut names: Vec<&String> = goldens.keys().collect();
    names.sort();
    for name in names {
        let want = &goldens[name];
        let got = ours.get(name).unwrap_or_else(|| {
            panic!(
                "the writer matrix produced no `{name}` row (it must produce all {})",
                goldens.len()
            )
        });
        assert_eq!(
            got, want,
            "`{name}`: this runtime's term_to_binary/1 image left the committed golden"
        );
    }
    assert_eq!(
        ours.len(),
        goldens.len(),
        "the writer matrix and the golden set must be the same size (ours: {}, goldens: {})",
        ours.len(),
        goldens.len()
    );
    let Some(oracle) = compiled_module_oracle() else {
        eprintln!(
            "SKIP: the system `erlc`/`erl` oracle is unavailable; the committed `MAP_EXT` \
             goldens still ran, only the live re-derivation is skipped"
        );
        return;
    };
    assert_eq!(
        oracle.len(),
        goldens.len(),
        "the live oracle printed {} rows where the golden set holds {}",
        oracle.len(),
        goldens.len()
    );
    for (name, want) in &goldens {
        let got = oracle.get(name).unwrap_or_else(|| {
            panic!(
                "the live oracle printed no `{name}` row ({} rows seen)",
                oracle.len()
            )
        });
        assert_eq!(
            got, want,
            "`{name}`: the committed golden differs from the live oracle\'s image              (the oracle changed, or the golden was hand-edited)"
        );
    }
}

/// Parses the committed `name=hex` golden file (`#` comments and blank lines ignored).
fn parse_goldens(text: &str) -> HashMap<String, String> {
    let mut rows = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, image) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("malformed golden row {line:?} (want `name=hex`)"));
        rows.insert(name.to_string(), image.to_string());
    }
    assert!(!rows.is_empty(), "the golden file must not be empty");
    rows
}

// ---------------------------------------------------------------------------
// Path-copying cost (host-independent: growth relations, never wall time)
// ---------------------------------------------------------------------------

/// The words a `maps:put/3` reserves do not grow with the map's size: the writer's own
/// path descent (`beam_term::map::map_put_cow_words_required`) answers a handful of
/// words at 250 pairs and at 2 500 pairs, while a from-scratch rebuild reserves the
/// whole map (`beam_term::map::map_words_capacity`).
///
/// The relation, not a number, is the pin: one extra trie level is
/// `[header][16 slots]` = 17 words, and the statement allows one level of growth plus a
/// word of slack. A writer that fell back to a rebuild would blow through it by three
/// orders of magnitude.
#[test]
fn a_put_reservation_does_not_grow_with_the_map() {
    run_sized(1 << 21, |reg, ctx| {
        let absent = int(-1);
        let mut required = Vec::new();
        for n in [250i64, LARGE_KEYS] {
            let pairs: Vec<(Eterm, Eterm)> = (1..=n).map(|k| (int(k), int(k * 10))).collect();
            let m = map_of(reg, ctx, &pairs);
            let words = map_put_cow_words_required(m, absent)
                .expect("the writer's own path descent accepts the map");
            assert!(
                words < 100,
                "{n} keys: a put of an absent key must stay inside a hundred words, got {words}"
            );
            required.push(words);
        }
        let (small, large) = (required[0], required[1]);
        assert!(
            large <= small + 18,
            "250 -> 2500 keys must not add more than one trie level: {small} -> {large} words"
        );

        let rebuild = map_words_capacity(LARGE_KEYS as usize);
        assert!(
            large * 50 < rebuild,
            "a path copy ({large} words) must stay far below the rebuild reservation \
             ({rebuild} words) for {LARGE_KEYS} keys"
        );
    });
}

/// The remove side reserves **nothing** for a key the map does not hold — the writer
/// writes no node, so `maps:remove/2` can answer its input map without allocating.
#[test]
fn a_remove_of_an_absent_key_reserves_nothing() {
    run_sized(1 << 21, |reg, ctx| {
        for n in [250i64, LARGE_KEYS] {
            let pairs: Vec<(Eterm, Eterm)> = (1..=n).map(|k| (int(k), int(k * 10))).collect();
            let m = map_of(reg, ctx, &pairs);
            assert_eq!(
                map_remove_cow_words_required(m, int(-1))
                    .expect("the writer's own path descent accepts the map"),
                0,
                "{n} keys: an absent key's removal writes nothing"
            );
            let present = map_remove_cow_words_required(m, int(1))
                .expect("the writer's own path descent accepts the map");
            assert!(
                present < 100,
                "{n} keys: a present key's removal must stay inside a hundred words, got {present}"
            );
        }
    });
}

/// The end-to-end statement, measured on the context's own heap: one `maps:put/3` on a
/// 2 500-key map writes a bounded number of words (`<= 100`), while a
/// `maps:from_list/1` rebuild of the very same map writes at least 50 000.
///
/// The figures are the heap's `used()` delta, so they are the words the BIF actually
/// reserved — and the assertion is the relation between the two, never an exact count.
#[test]
fn a_put_writes_bounded_words_where_a_rebuild_writes_the_whole_map() {
    run_sized(1 << 21, |reg, ctx| {
        let pairs: Vec<(Eterm, Eterm)> = (1..=LARGE_KEYS).map(|k| (int(k), int(k * 10))).collect();
        let m = map_of(reg, ctx, &pairs);
        let absent = int(-1);

        let before = ctx.heap().used();
        let grown = call(reg, ctx, "maps", "put", &[absent, int(1), m]);
        let path_copy = ctx.heap().used() - before;
        assert_eq!(
            size_of(reg, ctx, grown),
            LARGE_KEYS + 1,
            "the put inserted one pair"
        );
        assert!(
            path_copy <= 100,
            "a path copy on a {LARGE_KEYS}-key map must write at most 100 words, wrote {path_copy}"
        );

        // The same map through the retained from-scratch builder, over the same pairs.
        let tuples: Vec<Eterm> = pairs
            .iter()
            .map(|&(k, v)| ctx.tuple(&[k, v]).expect("a 2-tuple fits"))
            .collect();
        let list = ctx.list(&tuples).expect("the pair list fits");
        let before = ctx.heap().used();
        let rebuilt = call(reg, ctx, "maps", "from_list", &[list]);
        let rebuild = ctx.heap().used() - before;
        assert_eq!(
            size_of(reg, ctx, rebuilt),
            LARGE_KEYS,
            "the rebuild holds the same pairs"
        );
        assert!(
            rebuild >= 50_000,
            "a from_list/1 rebuild of {LARGE_KEYS} pairs must reserve the whole map \
             (>= 50 000 words), reserved {rebuild}"
        );
        assert!(
            path_copy * 500 < rebuild,
            "the path copy ({path_copy} words) must stay far below the rebuild ({rebuild} words)"
        );
    });
}
