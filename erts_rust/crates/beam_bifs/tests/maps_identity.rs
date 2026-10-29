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

//! The **no-op identity** of `maps:put/3`, `maps:update/3`, `maps:remove/2` and
//! `maps:merge/2`: a call whose result is already one of the argument terms returns
//! *that argument map term*, not a rebuilt copy. For `put`/`update`/`remove` the
//! deciding test is the value word; for `merge/2` it is ERTS's own rule — the empty
//! arms, plus "every key of `Map1` is in `Map2`" which hands back **`Map2`** — see the
//! matrix below.
//!
//! # The predicate: raw word equality
//!
//! ERTS decides "same value, same map" by comparing the two **encoded words**
//! (`erl_map.c`'s `erts_maps_put`/`erts_maps_update`, `found_key: if (*vs == value)`,
//! and the HAMT twin `erts_hashmap_insert_down`'s `*sz = 0` arm), so this suite
//! compares with [`Eterm`]'s `==` and never with the structural [`compare::eq`]. The
//! difference is deliberate and observable: two separately built `[1,2]` lists are
//! `=:=` but not the same *word*, and re-putting such a value must rebuild (a
//! structural comparison would wrongly hand the map back). The observer is
//! `erts_debug:same/2`, which is itself raw word equality (`beam_debug.c:64-68`;
//! `src/erts_internal.rs::same_2`).
//!
//! `=:=` never sees any of this, so the matrix below is the only pin: for every row
//! the test asserts the returned term is **word-identical to the named argument** or a
//! **different word**, and the content assertions keep the answers honest.
//!
//! # Oracle matrix (measured on the system OTP 29.1.2 / erts-17.1.1, compiled module,
//! `V = 42`, `M = #{a => V}`; every map built with `maps:from_list/1` over a
//! differently-ordered pair list so the compiler cannot CSE two expressions into one
//! term)
//!
//! ```text
//! erts_debug:same(maps:put(a,V,M), M)                        => true  (same value word)
//! erts_debug:same(maps:put(b,1,M), M)                        => false
//! erts_debug:same(maps:put(b,1,#{a=>1,b=>2}), #{a=>1,b=>2})   => false
//! erts_debug:same(maps:put(a,43,M), M)                       => false
//! erts_debug:same(maps:update(a,V,M), M)                     => true
//! erts_debug:same(maps:remove(zz,M), M)                      => true  (absent key)
//! erts_debug:same(maps:with([a],M), M)                       => false (negative pin)
//! ```
//!
//! # `maps:merge/2`'s identity arms are ERTS's (`erl_map.c:1256-1372`)
//!
//! They are a **key-set** rule, not a value rule, and they are measured:
//!
//! ```text
//! merge(M1, M1)                                       => Map1  (any size, flat or HAMT)
//! merge(M, #{})                                       => M1    (empty Map2)
//! merge(#{}, M)                                       => M2    (empty Map1)
//! merge(#{a=>1}, #{a=>99})                            => Map2  (shared key, Map2 wins)
//! merge(#{a=>1,b=>2}, #{b=>2,a=>1})                   => Map2  (same key set)
//! merge(#{a=>1}, #{a=>1,b=>2})                        => Map2  (Map1 ⊆ Map2)
//! merge(#{a=>1,b=>2}, #{a=>1})                        => fresh (Map2 ⊂ Map1: n > n2)
//! merge(#{a=>1,b=>2}, #{a=>99})                       => fresh (a key of Map1 missing)
//! merge(40-key, 41-key superset)                      => Map2  (HAMT)
//! merge(20-key, 40-key superset)                      => Map2  (HAMT)
//! merge(40-key, 40-key with different values)         => Map2  (HAMT, values ignored)
//! merge(3-key flat, 40-key HAMT superset)             => Map2  (mixed representations)
//! ```
//!
//! `flatmap_merge` reaches the shared-key-set arm as `if (n == n2) return map2;`
//! (`erl_map.c:1368-1372`), the HAMT merge as "only unique B stuff" at the root — so
//! the values never matter (`same(merge(#{a=>1},#{a=>99}), Map2)` ⇒ `true`) and the
//! answer for an identical key set is **`Map2`**, never `Map1`.

use std::collections::HashMap;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{compare, make_small, signed_value, AtomTable, Eterm};

/// The atoms these cases name (all pre-seeded: no `AtomInterner` seam is installed,
/// so an unseeded name would not resolve).
const ATOMS: [&str; 10] = [
    "true", "false", "ok", "error", "none", "a", "b", "zz", "badmap", "badkey",
];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(ATOMS);
    atoms
}

/// Runs `f` with the real [`default_registry`] over a fresh heap and the seeded atom
/// table — no optional seams installed (none of these BIFs needs one).
fn run<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 18).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx)
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

/// `maps:new/0` — a **fresh** empty map term on the test heap.
fn empty(reg: &BifRegistry, ctx: &mut BifContext<'_>) -> Eterm {
    call(reg, ctx, "maps", "new", &[])
}

/// `maps:from_list/1` over already-built terms.
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

/// The value `Key` is bound to (`maps:get/2`, exact-key).
fn get(reg: &BifRegistry, ctx: &mut BifContext<'_>, key: Eterm, map: Eterm) -> Eterm {
    call(reg, ctx, "maps", "get", &[key, map])
}

/// The one predicate this suite is about: the two terms are **the same word**, never
/// merely `=:=` (see the module docs).
fn same_word(a: Eterm, b: Eterm) -> bool {
    a == b
}

/// Asserts `result` is the oracle's structured `error:{Tag, Value}` raise.
#[track_caller]
fn assert_reason(
    result: Result<Eterm>,
    ctx: &mut BifContext<'_>,
    tag: &str,
    value: Eterm,
) {
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

// ---------------------------------------------------------------------------
// maps:put/3
// ---------------------------------------------------------------------------

#[test]
fn put_hands_back_the_map_when_the_value_word_is_unchanged() {
    run(|reg, ctx| {
        let (a, b) = (atom(ctx, "a"), atom(ctx, "b"));
        let v = int(42);
        let m = map_of(reg, ctx, &[(a, v)]);

        // maps:put(a, V, M) ⇒ M itself (the C's `if (*vs == value) return map`).
        let got = call(reg, ctx, "maps", "put", &[a, v, m]);
        assert!(same_word(got, m), "maps:put/3 with the same value word");
        assert_eq!(size_of(reg, ctx, got), 1, "still one pair");

        // A new key changes the key set, so the answer is a fresh map.
        let got = call(reg, ctx, "maps", "put", &[b, int(1), m]);
        assert!(!same_word(got, m), "maps:put/3 with a new key");
        assert_eq!(size_of(reg, ctx, got), 2);

        // A different value word changes the value.
        let got = call(reg, ctx, "maps", "put", &[a, int(43), m]);
        assert!(!same_word(got, m), "maps:put/3 with a new value");
        assert!(same_word(get(reg, ctx, a, got), int(43)), "the new value");

        // No whole-map identity for a differing key set either.
        let two = map_of(reg, ctx, &[(a, int(1)), (b, int(2))]);
        let got = call(reg, ctx, "maps", "put", &[b, int(1), two]);
        assert!(!same_word(got, two), "maps:put/3 b on #{{a=>1,b=>2}}");
        assert_eq!(size_of(reg, ctx, got), 2);
    });
}

#[test]
fn put_compares_value_words_not_structure() {
    run(|reg, ctx| {
        let a = atom(ctx, "a");
        // Two structurally equal (`=:=`) but *distinct* list words: `[1,2]` built twice.
        let elems = [int(1), int(2)];
        let first = ctx.list(&elems).expect("fits");
        let second = ctx.list(&elems).expect("fits");
        assert!(
            !same_word(first, second),
            "the two list words must be distinct heap terms"
        );
        let m = map_of(reg, ctx, &[(a, first)]);

        // The new value is `=:=` to the old one but a different word, so the oracle
        // rebuilds — a structural comparison would wrongly return `m`.
        let got = call(reg, ctx, "maps", "put", &[a, second, m]);
        assert!(
            !same_word(got, m),
            "a structurally equal but distinct value word must rebuild"
        );
        assert!(
            compare::eq(get(reg, ctx, a, got), second, ctx.atoms()),
            "the rebuilt map holds the new value"
        );

        // The very same word is the no-op case.
        let got = call(reg, ctx, "maps", "put", &[a, first, m]);
        assert!(same_word(got, m), "the same value word is a no-op");
    });
}

// ---------------------------------------------------------------------------
// maps:update/3
// ---------------------------------------------------------------------------

#[test]
fn update_hands_back_the_map_when_the_value_word_is_unchanged() {
    run(|reg, ctx| {
        let (a, b) = (atom(ctx, "a"), atom(ctx, "b"));
        let v = int(42);
        let m = map_of(reg, ctx, &[(a, v), (b, int(7))]);

        let got = call(reg, ctx, "maps", "update", &[a, v, m]);
        assert!(same_word(got, m), "maps:update/3 with the same value word");

        let got = call(reg, ctx, "maps", "update", &[a, int(43), m]);
        assert!(!same_word(got, m), "maps:update/3 with a new value");
        assert!(same_word(get(reg, ctx, a, got), int(43)));
        assert!(same_word(get(reg, ctx, b, got), int(7)), "b is untouched");

        // A structurally equal, distinct value word rebuilds (word equality).
        let elems = [int(1), int(2)];
        let first = ctx.list(&elems).expect("fits");
        let second = ctx.list(&elems).expect("fits");
        let listed = map_of(reg, ctx, &[(a, first)]);
        let got = call(reg, ctx, "maps", "update", &[a, second, listed]);
        assert!(!same_word(got, listed), "distinct value words rebuild");

        // An absent key is the oracle's `error:{badkey, Key}` — the early return must
        // not shadow it.
        let zz = atom(ctx, "zz");
        let bad = invoke(reg, ctx, "maps", "update", &[zz, v, listed]);
        assert_reason(bad, ctx, "badkey", zz);
    });
}

// ---------------------------------------------------------------------------
// maps:remove/2
// ---------------------------------------------------------------------------

#[test]
fn remove_hands_back_the_map_when_the_key_is_absent() {
    run(|reg, ctx| {
        let (a, b, zz) = (atom(ctx, "a"), atom(ctx, "b"), atom(ctx, "zz"));
        let m = map_of(reg, ctx, &[(a, int(1)), (b, int(2))]);

        // An absent key removes nothing (`erts_maps_take`'s `*res = map`).
        let got = call(reg, ctx, "maps", "remove", &[zz, m]);
        assert!(same_word(got, m), "maps:remove/2 of an absent key");
        assert_eq!(size_of(reg, ctx, got), 2);

        // A present key always rebuilds.
        let got = call(reg, ctx, "maps", "remove", &[a, m]);
        assert!(!same_word(got, m), "maps:remove/2 of a present key");
        assert_eq!(size_of(reg, ctx, got), 1);
    });
}

// ---------------------------------------------------------------------------
// maps:merge/2
// ---------------------------------------------------------------------------

#[test]
fn merge_hands_back_an_argument_on_the_empty_arms() {
    run(|reg, ctx| {
        let (a, b) = (atom(ctx, "a"), atom(ctx, "b"));
        let v = int(42);
        let m = map_of(reg, ctx, &[(a, v)]);

        // An empty Map2 contributes nothing ⇒ Map1 (`if (n2 == 0) return map1`).
        let blank = empty(reg, ctx);
        let got = call(reg, ctx, "maps", "merge", &[m, blank]);
        assert!(same_word(got, m), "maps:merge/2 with an empty Map2");

        // An empty Map1 means the union *is* Map2 (`if (n1 == 0) return map2`).
        let blank = empty(reg, ctx);
        let got = call(reg, ctx, "maps", "merge", &[blank, m]);
        assert!(same_word(got, m), "maps:merge/2 with an empty Map1");

        // Both empty: Map2 is the answer, and here that is the *second* argument word.
        let blank1 = empty(reg, ctx);
        let blank2 = empty(reg, ctx);
        assert!(!same_word(blank1, blank2), "two distinct empty map terms");
        let got = call(reg, ctx, "maps", "merge", &[blank1, blank2]);
        assert!(same_word(got, blank2), "merge of two empty maps is Map2");

        // A new key in Map2 is a real merge.
        let extra = map_of(reg, ctx, &[(b, int(1))]);
        let got = call(reg, ctx, "maps", "merge", &[m, extra]);
        assert!(!same_word(got, m), "maps:merge/2 that adds a key");
        assert!(!same_word(got, extra), "…and is not Map2 either");
        assert_eq!(size_of(reg, ctx, got), 2);

        // A changed value on a shared key: the key sets are equal, so Map2 is the
        // answer (`if (n == n2) return map2`) — never Map1.
        let changed = map_of(reg, ctx, &[(a, int(43))]);
        let got = call(reg, ctx, "maps", "merge", &[m, changed]);
        assert!(!same_word(got, m), "maps:merge/2 that changes a value");
        assert!(same_word(got, changed), "Map2 itself, values and all");
        assert!(same_word(get(reg, ctx, a, got), int(43)), "Map2 wins");
    });
}

/// `maps:merge/2`'s identity arm is a **key-set** test, never a value test
/// (`erl_map.c:1368-1372`): `Map2`'s value wins on a shared key, so whenever `Map2`
/// already holds every key of `Map1` the answer is `Map2` itself whatever the values
/// are — word-identical or not.
#[test]
fn merge_ignores_value_words_entirely() {
    run(|reg, ctx| {
        let a = atom(ctx, "a");
        let elems = [int(1), int(2)];
        let first = ctx.list(&elems).expect("fits");
        let second = ctx.list(&elems).expect("fits");
        let m = map_of(reg, ctx, &[(a, first)]);

        // `Map2`'s value is `=:=` but a different word: still `Map2` (rule 5).
        let twin = map_of(reg, ctx, &[(a, second)]);
        let got = call(reg, ctx, "maps", "merge", &[m, twin]);
        assert!(!same_word(got, m), "the answer is not Map1");
        assert!(same_word(got, twin), "the answer is Map2 itself");
        assert!(
            compare::eq(get(reg, ctx, a, got), second, ctx.atoms()),
            "Map2's value wins"
        );

        // The same value word: the same answer, `Map2`.
        let echo = map_of(reg, ctx, &[(a, first)]);
        let got = call(reg, ctx, "maps", "merge", &[m, echo]);
        assert!(same_word(got, echo), "the same value word still answers Map2");
        assert!(!same_word(got, m), "…and not Map1");
    });
}

/// `flatmap_merge`'s `n == n2` arm (`erl_map.c:1368-1372`) — and its HAMT twin — hand
/// back **`Map2`** for two *distinct* maps with the same key set, whether the values
/// agree or not. The reverse relation is *not* an identity: with `Map2 ⊂ Map1` the
/// union is larger than `Map2`, so neither argument is the answer.
#[test]
fn merge_with_an_identical_key_set_returns_map2() {
    run(|reg, ctx| {
        let (a, b) = (atom(ctx, "a"), atom(ctx, "b"));
        let v = int(42);

        // Same key set, identical values, distinct terms.
        let m1 = map_of(reg, ctx, &[(a, v)]);
        let m2 = map_of(reg, ctx, &[(a, v)]);
        assert!(!same_word(m1, m2), "distinct heap maps");
        let got = call(reg, ctx, "maps", "merge", &[m1, m2]);
        assert!(same_word(got, m2), "Map2 (`if (n == n2) return map2`)");
        assert!(!same_word(got, m1), "not Map1");
        assert!(
            compare::eq(got, m2, ctx.atoms()),
            "the content is Map2's either way"
        );

        // Same key set, distinct values, distinct terms (order of construction
        // reversed so the two terms cannot coincide).
        let one = map_of(reg, ctx, &[(a, int(1)), (b, int(2))]);
        let swapped = map_of(reg, ctx, &[(b, int(2)), (a, int(1))]);
        assert!(!same_word(one, swapped), "distinct heap maps");
        let got = call(reg, ctx, "maps", "merge", &[one, swapped]);
        assert!(same_word(got, swapped), "same key set ⇒ Map2");
        assert!(!same_word(got, one), "…not Map1");

        // `Map1 ⊆ Map2` (a proper subset this time): still Map2.
        let small = map_of(reg, ctx, &[(a, v)]);
        let big = map_of(reg, ctx, &[(a, v), (b, int(7))]);
        let got = call(reg, ctx, "maps", "merge", &[small, big]);
        assert!(same_word(got, big), "Map1 ⊆ Map2 ⇒ Map2");

        // `Map2 ⊂ Map1`: the union is neither argument — a fresh map.
        let got = call(reg, ctx, "maps", "merge", &[big, small]);
        assert!(!same_word(got, big), "Map2 ⊂ Map1 ⇒ not Map1");
        assert!(!same_word(got, small), "Map2 ⊂ Map1 ⇒ not Map2");
        assert_eq!(size_of(reg, ctx, got), 2, "the union still has both keys");
        assert!(
            compare::eq(got, big, ctx.atoms()),
            "content equals the superset"
        );
    });
}

// ---------------------------------------------------------------------------
// HAMT (a map above MAP_SMALL_MAP_LIMIT = 32) — same arms, both representations
// ---------------------------------------------------------------------------

#[test]
fn the_no_op_identity_holds_for_a_hamt_too() {
    run(|reg, ctx| {
        let pairs: Vec<(i64, i64)> = (1..=40).map(|n| (n, n)).collect();
        let hamt = map_of_ints(reg, ctx, &pairs);
        assert_eq!(size_of(reg, ctx, hamt), 40);

        // put: the same value word for an existing key is a no-op.
        let got = call(reg, ctx, "maps", "put", &[int(7), int(7), hamt]);
        assert!(same_word(got, hamt), "maps:put/3 on a HAMT, same value");
        // …a new value word is not.
        let got = call(reg, ctx, "maps", "put", &[int(7), int(8), hamt]);
        assert!(!same_word(got, hamt), "maps:put/3 on a HAMT, new value");

        // update: the same value word is a no-op, a new one is not.
        let got = call(reg, ctx, "maps", "update", &[int(7), int(7), hamt]);
        assert!(same_word(got, hamt), "maps:update/3 on a HAMT, same value");
        let got = call(reg, ctx, "maps", "update", &[int(7), int(8), hamt]);
        assert!(!same_word(got, hamt), "maps:update/3 on a HAMT, new value");

        // remove: an absent key is a no-op, a present one is not.
        let got = call(reg, ctx, "maps", "remove", &[int(999), hamt]);
        assert!(same_word(got, hamt), "maps:remove/2 of an absent HAMT key");
        let got = call(reg, ctx, "maps", "remove", &[int(7), hamt]);
        assert!(!same_word(got, hamt), "maps:remove/2 of a present HAMT key");
        assert_eq!(size_of(reg, ctx, got), 39);

        // merge: the empty arms on both sides.
        let blank = empty(reg, ctx);
        let got = call(reg, ctx, "maps", "merge", &[hamt, blank]);
        assert!(same_word(got, hamt), "merge(HAMT, {{}})");
        let blank = empty(reg, ctx);
        let got = call(reg, ctx, "maps", "merge", &[blank, hamt]);
        assert!(same_word(got, hamt), "merge({{}}, HAMT)");
        // …a self-merge (rule 1)…
        let got = call(reg, ctx, "maps", "merge", &[hamt, hamt]);
        assert!(same_word(got, hamt), "merge(HAMT, HAMT)");
        // …a `Map1 ⊆ Map2` pair, which answers Map2 even though Map2 is the *larger*
        // term (`if (n == n2) return map2` / the HAMT merge's "only unique B")…
        let superset_pairs: Vec<(i64, i64)> = (1..=41).map(|n| (n, n)).collect();
        let superset = map_of_ints(reg, ctx, &superset_pairs);
        let got = call(reg, ctx, "maps", "merge", &[hamt, superset]);
        assert!(same_word(got, superset), "merge(HAMT40, a 41-key superset)");
        // …and a real one.
        let extra = map_of_ints(reg, ctx, &[(99, 99)]);
        let got = call(reg, ctx, "maps", "merge", &[hamt, extra]);
        assert!(!same_word(got, hamt), "merge(HAMT, a new key)");
        assert!(!same_word(got, extra), "…and not Map2 either");
        assert_eq!(size_of(reg, ctx, got), 41);
    });
}

/// The `n == n2` arm on the HAMT tier and across representations: a `Map2` that already
/// holds every key of `Map1` is the answer — 20 keys into a 40-key superset, a 40-key
/// map with **different values** (the values are irrelevant), and a 3-key *flat* map
/// into a 40-key HAMT (the C's `map_merge_mixed` path).
#[test]
fn merge_answers_map2_for_a_subset_on_the_hamt_tier_too() {
    run(|reg, ctx| {
        let keys = |from: i64, to: i64| -> Vec<(i64, i64)> {
            (from..=to).map(|n| (n, n)).collect()
        };

        // 20-key ⊂ 40-key, both distinct HAMT terms.
        let twenty = map_of_ints(reg, ctx, &keys(1, 20));
        let forty = map_of_ints(reg, ctx, &keys(1, 40));
        assert!(!same_word(twenty, forty), "distinct heap maps");
        let got = call(reg, ctx, "maps", "merge", &[twenty, forty]);
        assert!(same_word(got, forty), "merge(20-key, 40-key superset) ⇒ Map2");
        assert!(!same_word(got, twenty), "…not Map1");

        // Same 40 keys, every value different: still Map2 (a key-set test).
        let values: Vec<(i64, i64)> = (1..=40).map(|n| (n, n * 1000)).collect();
        let other = map_of_ints(reg, ctx, &values);
        let got = call(reg, ctx, "maps", "merge", &[forty, other]);
        assert!(same_word(got, other), "merge(40-key, 40-key new values) ⇒ Map2");
        assert!(!same_word(got, forty), "…not Map1");

        // A flat map whose keys all live in a HAMT (mixed representations).
        let flat = map_of_ints(reg, ctx, &keys(1, 3));
        assert_eq!(size_of(reg, ctx, flat), 3, "still a flat map");
        let got = call(reg, ctx, "maps", "merge", &[flat, forty]);
        assert!(same_word(got, forty), "merge(3-key flat, 40-key HAMT) ⇒ Map2");
        assert!(!same_word(got, flat), "…not Map1");

        // The reverse relation on the mixed pair is a fresh map again.
        let got = call(reg, ctx, "maps", "merge", &[forty, flat]);
        assert!(!same_word(got, forty), "merge(40-key HAMT, 3-key flat) ⇒ fresh");
        assert!(!same_word(got, flat), "…not Map2");
        assert_eq!(size_of(reg, ctx, got), 40);
    });
}

// ---------------------------------------------------------------------------
// Error paths are unchanged (order and reason)
// ---------------------------------------------------------------------------

#[test]
fn the_error_paths_are_unchanged_by_the_early_returns() {
    run(|reg, ctx| {
        let nine = int(9);
        let two = int(2);
        let one_map = map_of_ints(reg, ctx, &[(1, 1)]);
        let blank = empty(reg, ctx);

        // put/update/remove: `Map` is the LAST argument.
        let bad = invoke(reg, ctx, "maps", "put", &[nine, two, nine]);
        assert_reason(bad, ctx, "badmap", nine);
        let bad = invoke(reg, ctx, "maps", "update", &[nine, two, nine]);
        assert_reason(bad, ctx, "badmap", nine);
        let bad = invoke(reg, ctx, "maps", "remove", &[nine, nine]);
        assert_reason(bad, ctx, "badmap", nine);

        // merge: each argument is named in turn, `Map1` first (the pre-existing order).
        let bad = invoke(reg, ctx, "maps", "merge", &[nine, one_map]);
        assert_reason(bad, ctx, "badmap", nine);
        let bad = invoke(reg, ctx, "maps", "merge", &[one_map, two]);
        assert_reason(bad, ctx, "badmap", two);
        let bad = invoke(reg, ctx, "maps", "merge", &[nine, two]);
        assert_reason(bad, ctx, "badmap", nine);
        // …including the *same* non-map word twice, which rule 1 must not mistake for
        // a map merge (measured: `merge(notamap, notamap)` ⇒ `error:{badmap,notamap}`).
        let bad = invoke(reg, ctx, "maps", "merge", &[nine, nine]);
        assert_reason(bad, ctx, "badmap", nine);
        // …and an empty-map argument does not skip the classification of the other.
        let bad = invoke(reg, ctx, "maps", "merge", &[nine, blank]);
        assert_reason(bad, ctx, "badmap", nine);
        let blank = empty(reg, ctx);
        let bad = invoke(reg, ctx, "maps", "merge", &[blank, nine]);
        assert_reason(bad, ctx, "badmap", nine);
    });
}

// ---------------------------------------------------------------------------
// Negative pin: `maps:with/2` has no identity
// ---------------------------------------------------------------------------

#[test]
fn with_does_not_hand_back_the_map() {
    run(|reg, ctx| {
        let a = atom(ctx, "a");
        let m = map_of(reg, ctx, &[(a, int(42))]);
        let keys = ctx.list(&[a]).expect("fits");
        let got = call(reg, ctx, "maps", "with", &[keys, m]);
        assert!(
            !same_word(got, m),
            "maps:with/2 always builds (`maps.erl`'s with_1/2)"
        );
        assert!(compare::eq(got, m, ctx.atoms()), "though the content matches");
    });
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------

/// The compiled probe module. It is **compiled** on purpose: an `-eval` expression is
/// run by `erl_eval`, whose literal handling is not a compiled module's, and every map
/// here is built at runtime with `maps:from_list/1` over a pair list in a *different*
/// order from its twin — so no two expressions can be CSE'd into one term. The
/// `guard_*` rows prove the pairs the `merge_*` rows compare really are distinct terms,
/// which is what makes `erts_debug:same/2` able to tell `Map1` from `Map2` at all.
const ORACLE_MODULE: &str = r#"
-module(map_identity_oracle).
-export([go/0]).

go() ->
    Value = 42,
    A = list_to_atom("a"),
    B = list_to_atom("b"),
    Z = list_to_atom("zz"),
    M = maps:put(A, Value, maps:new()),
    %% `#{a => 1, b => 2}` — putting `1` under `b` is a real change (2 -> 1).
    Two = maps:put(B, 2, maps:put(A, 1, maps:new())),
    %% Distinct maps with the very same content (no literal sharing).
    Echo = maps:from_list([{A, Value}]),
    TwoR = maps:from_list([{B, 2}, {A, 1}]),
    One  = maps:from_list([{A, 1}]),
    High = maps:from_list([{A, 99}]),
    Pair = maps:from_list([{A, 1}, {B, 2}]),
    %% The HAMT tier (above MAP_SMALL_MAP_LIMIT = 32), built key-descending.
    K  = fun(N) -> list_to_atom("k" ++ integer_to_list(N)) end,
    Hn = fun(N, F) -> maps:from_list([{K(I), F(I)} || I <- lists:seq(N, 1, -1)]) end,
    H20  = Hn(20, fun(I) -> I end),
    H40  = Hn(40, fun(I) -> I end),
    H41  = maps:from_list([{K(41), 41} | [{K(I), I} || I <- lists:seq(40, 1, -1)]]),
    H40v = Hn(40, fun(I) -> I * 1000 end),
    Flat3 = maps:from_list([{K(3), 3}, {K(2), 2}, {K(1), 1}]),
    NotAMap = list_to_atom("notamap"),
    Rows = [
        {"put_same_value",         is(maps:put(A, Value, M), M)},
        {"put_new_key",            is(maps:put(B, 1, M), M)},
        {"put_into_two",           is(maps:put(B, 1, Two), Two)},
        {"put_new_value",          is(maps:put(A, 43, M), M)},
        {"update_same_value",      is(maps:update(A, Value, M), M)},
        {"guard_m_echo_distinct",       distinct(M, Echo)},
        {"guard_pair_twor_distinct",    distinct(Pair, TwoR)},
        {"guard_one_high_distinct",     distinct(One, High)},
        {"guard_h20_h40_distinct",      distinct(H20, H40)},
        {"guard_h40_h40v_distinct",     distinct(H40, H40v)},
        {"guard_flat3_h40_distinct",    distinct(Flat3, H40)},
        {"merge_self_is_map1",                   is(maps:merge(M, M), M)},
        {"merge_self_hamt_is_map1",              is(maps:merge(H40, H40), H40)},
        {"merge_empty_second",                   is(maps:merge(M, maps:new()), M)},
        {"merge_empty_first",                    is(maps:merge(maps:new(), M), M)},
        {"merge_shared_key_is_map2",             is(maps:merge(One, High), High)},
        {"merge_shared_key_is_map1",             is(maps:merge(One, High), One)},
        {"merge_same_set_is_map2",               is(maps:merge(M, Echo), Echo)},
        {"merge_same_set_is_map1",               is(maps:merge(M, Echo), M)},
        {"merge_two_same_set_is_map2",           is(maps:merge(Pair, TwoR), TwoR)},
        {"merge_two_same_set_is_map1",           is(maps:merge(Pair, TwoR), Pair)},
        {"merge_subset_is_map2",                 is(maps:merge(One, Pair), Pair)},
        {"merge_subset_is_map1",                 is(maps:merge(One, Pair), One)},
        {"merge_superset_is_map1",               is(maps:merge(Pair, One), Pair)},
        {"merge_superset_is_map2",               is(maps:merge(Pair, One), One)},
        {"merge_adds_key",                       is(maps:merge(M, maps:put(B, 1, maps:new())), M)},
        {"merge_hamt_superset41_is_map2",        is(maps:merge(H40, H41), H41)},
        {"merge_hamt_superset41_is_map1",        is(maps:merge(H40, H41), H40)},
        {"merge_hamt_20of40_is_map2",            is(maps:merge(H20, H40), H40)},
        {"merge_hamt_20of40_is_map1",            is(maps:merge(H20, H40), H20)},
        {"merge_hamt_sameset_newvalues_is_map2", is(maps:merge(H40, H40v), H40v)},
        {"merge_hamt_sameset_newvalues_is_map1", is(maps:merge(H40, H40v), H40)},
        {"merge_mixed_flat_hamt_is_map2",        is(maps:merge(Flat3, H40), H40)},
        {"merge_mixed_flat_hamt_is_map1",        is(maps:merge(Flat3, H40), Flat3)},
        {"err_same_non_map",       err(fun() -> maps:merge(NotAMap, NotAMap) end, NotAMap)},
        {"err_first_non_map",      err(fun() -> maps:merge(NotAMap, maps:new()) end, NotAMap)},
        {"err_second_non_map",     err(fun() -> maps:merge(M, NotAMap) end, NotAMap)},
        {"remove_absent",          is(maps:remove(Z, M), M)},
        {"with_one_key",           is(maps:with([A], M), M)}
    ],
    [io:format("~s=~s~n", [Name, atom_to_list(Answer)]) || {Name, Answer} <- Rows],
    ok.

is(X, Y) -> erts_debug:same(X, Y).
distinct(X, Y) -> not is(X, Y).
err(F, Expected) ->
    try F() of
        _ -> false
    catch
        error:R -> R =:= {badmap, Expected}
    end.
"#;

/// Compiles [`ORACLE_MODULE`] with the system `erlc`, runs it with the system `erl`
/// and returns its `name=true|false` lines. `None` when the toolchain cannot be
/// spawned (the caller then skips loudly); a toolchain that *is* present but fails
/// panics, so the check can never pass vacuously.
fn compiled_module_oracle() -> Option<HashMap<String, bool>> {
    let dir = std::env::temp_dir().join("beam_bifs_maps_identity_oracle");
    std::fs::create_dir_all(&dir).ok()?;
    let source = dir.join("map_identity_oracle.erl");
    std::fs::write(&source, ORACLE_MODULE).ok()?;
    let compiled = std::process::Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output()
        .ok()?;
    assert!(
        compiled.status.success(),
        "erlc could not compile the oracle fixture:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let run = std::process::Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-eval")
        .arg("map_identity_oracle:go(), halt(0).")
        .output()
        .ok()?;
    assert!(
        run.status.success(),
        "the oracle fixture failed to run:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let mut answers = HashMap::new();
    for line in String::from_utf8_lossy(&run.stdout).lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let answer = match value.trim() {
            "true" => true,
            "false" => false,
            other => panic!("the oracle printed a non-boolean `{line}`: {other}"),
        };
        answers.insert(name.to_string(), answer);
    }
    Some(answers)
}

/// Re-derives the whole matrix from the live system `erl` and compares it with this
/// runtime's own answers, row for row. Skips LOUDLY when `erlc`/`erl` is unavailable
/// (CI's `cargo test` job has no oracle).
///
/// The `merge_*` rows are ERTS's key-set rule (`erl_map.c:1256-1372`), so **every** row
/// must agree: the identical-key-set family answers `Map2`, the `Map2 ⊂ Map1` family a
/// fresh map, and no row is a documented divergence.
#[test]
fn the_identity_matrix_agrees_with_the_live_oracle() {
    let Some(oracle) = compiled_module_oracle() else {
        eprintln!(
            "SKIP: the system `erlc`/`erl` oracle is unavailable or does not run; \
             the maps no-op-identity cross-check is skipped"
        );
        return;
    };

    // The oracle's own answers — the transcript this suite was built from (OTP 29.1.2 /
    // erts-17.1.1). Asserted against the live run *and* against this runtime below.
    let expected: &[(&str, bool)] = &[
        ("put_same_value", true),
        ("put_new_key", false),
        ("put_into_two", false),
        ("put_new_value", false),
        ("update_same_value", true),
        ("guard_m_echo_distinct", true),
        ("guard_pair_twor_distinct", true),
        ("guard_one_high_distinct", true),
        ("guard_h20_h40_distinct", true),
        ("guard_h40_h40v_distinct", true),
        ("guard_flat3_h40_distinct", true),
        ("merge_self_is_map1", true),
        ("merge_self_hamt_is_map1", true),
        ("merge_empty_second", true),
        ("merge_empty_first", true),
        ("merge_shared_key_is_map2", true),
        ("merge_shared_key_is_map1", false),
        ("merge_same_set_is_map2", true),
        ("merge_same_set_is_map1", false),
        ("merge_two_same_set_is_map2", true),
        ("merge_two_same_set_is_map1", false),
        ("merge_subset_is_map2", true),
        ("merge_subset_is_map1", false),
        ("merge_superset_is_map1", false),
        ("merge_superset_is_map2", false),
        ("merge_adds_key", false),
        ("merge_hamt_superset41_is_map2", true),
        ("merge_hamt_superset41_is_map1", false),
        ("merge_hamt_20of40_is_map2", true),
        ("merge_hamt_20of40_is_map1", false),
        ("merge_hamt_sameset_newvalues_is_map2", true),
        ("merge_hamt_sameset_newvalues_is_map1", false),
        ("merge_mixed_flat_hamt_is_map2", true),
        ("merge_mixed_flat_hamt_is_map1", false),
        ("err_same_non_map", true),
        ("err_first_non_map", true),
        ("err_second_non_map", true),
        ("remove_absent", true),
        ("with_one_key", false),
    ];
    for (name, answer) in expected {
        assert_eq!(
            oracle.get(*name).copied(),
            Some(*answer),
            "the live oracle's `{name}` row"
        );
    }

    let keys = |from: i64, to: i64| -> Vec<(i64, i64)> {
        (from..=to).map(|n| (n, n)).collect()
    };

    // This runtime's answers, observed as word identity through the real registry —
    // the same rows, so the two columns are comparable.
    run(|reg, ctx| {
        let (a, b, zz) = (atom(ctx, "a"), atom(ctx, "b"), atom(ctx, "zz"));
        let nine = int(9);
        let v = int(42);
        let m = map_of(reg, ctx, &[(a, v)]);
        let two = map_of(reg, ctx, &[(a, int(1)), (b, int(2))]);
        let echo = map_of(reg, ctx, &[(a, v)]);
        let two_r = map_of(reg, ctx, &[(b, int(2)), (a, int(1))]);
        let one = map_of(reg, ctx, &[(a, int(1))]);
        let high = map_of(reg, ctx, &[(a, int(99))]);
        let pair = map_of(reg, ctx, &[(a, int(1)), (b, int(2))]);
        let blank = empty(reg, ctx);
        let blank2 = empty(reg, ctx);
        let extra = map_of(reg, ctx, &[(b, int(1))]);
        let h20 = map_of_ints(reg, ctx, &keys(1, 20));
        let h40 = map_of_ints(reg, ctx, &keys(1, 40));
        let h41 = map_of_ints(reg, ctx, &keys(1, 41));
        let h40v: Vec<(i64, i64)> = (1..=40).map(|n| (n, n * 1000)).collect();
        let h40v = map_of_ints(reg, ctx, &h40v);
        let flat3 = map_of_ints(reg, ctx, &keys(1, 3));
        let key_list = ctx.list(&[a]).expect("fits");
        let blank_empty = empty(reg, ctx);

        let merge = |ctx: &mut BifContext<'_>, x: Eterm, y: Eterm| -> Eterm {
            call(reg, ctx, "maps", "merge", &[x, y])
        };
        let rows: Vec<(&str, bool)> = vec![
            ("put_same_value", same_word(call(reg, ctx, "maps", "put", &[a, v, m]), m)),
            ("put_new_key", same_word(call(reg, ctx, "maps", "put", &[b, int(1), m]), m)),
            ("put_into_two", same_word(call(reg, ctx, "maps", "put", &[b, int(1), two]), two)),
            ("put_new_value", same_word(call(reg, ctx, "maps", "put", &[a, int(43), m]), m)),
            (
                "update_same_value",
                same_word(call(reg, ctx, "maps", "update", &[a, v, m]), m),
            ),
            ("guard_m_echo_distinct", !same_word(m, echo)),
            ("guard_pair_twor_distinct", !same_word(pair, two_r)),
            ("guard_one_high_distinct", !same_word(one, high)),
            ("guard_h20_h40_distinct", !same_word(h20, h40)),
            ("guard_h40_h40v_distinct", !same_word(h40, h40v)),
            ("guard_flat3_h40_distinct", !same_word(flat3, h40)),
            ("merge_self_is_map1", same_word(merge(ctx, m, m), m)),
            ("merge_self_hamt_is_map1", same_word(merge(ctx, h40, h40), h40)),
            ("merge_empty_second", same_word(merge(ctx, m, blank), m)),
            ("merge_empty_first", same_word(merge(ctx, blank2, m), m)),
            ("merge_shared_key_is_map2", same_word(merge(ctx, one, high), high)),
            ("merge_shared_key_is_map1", same_word(merge(ctx, one, high), one)),
            ("merge_same_set_is_map2", same_word(merge(ctx, m, echo), echo)),
            ("merge_same_set_is_map1", same_word(merge(ctx, m, echo), m)),
            ("merge_two_same_set_is_map2", same_word(merge(ctx, pair, two_r), two_r)),
            ("merge_two_same_set_is_map1", same_word(merge(ctx, pair, two_r), pair)),
            ("merge_subset_is_map2", same_word(merge(ctx, one, pair), pair)),
            ("merge_subset_is_map1", same_word(merge(ctx, one, pair), one)),
            ("merge_superset_is_map1", same_word(merge(ctx, pair, one), pair)),
            ("merge_superset_is_map2", same_word(merge(ctx, pair, one), one)),
            ("merge_adds_key", same_word(merge(ctx, m, extra), m)),
            ("merge_hamt_superset41_is_map2", same_word(merge(ctx, h40, h41), h41)),
            ("merge_hamt_superset41_is_map1", same_word(merge(ctx, h40, h41), h40)),
            ("merge_hamt_20of40_is_map2", same_word(merge(ctx, h20, h40), h40)),
            ("merge_hamt_20of40_is_map1", same_word(merge(ctx, h20, h40), h20)),
            (
                "merge_hamt_sameset_newvalues_is_map2",
                same_word(merge(ctx, h40, h40v), h40v),
            ),
            (
                "merge_hamt_sameset_newvalues_is_map1",
                same_word(merge(ctx, h40, h40v), h40),
            ),
            ("merge_mixed_flat_hamt_is_map2", same_word(merge(ctx, flat3, h40), h40)),
            ("merge_mixed_flat_hamt_is_map1", same_word(merge(ctx, flat3, h40), flat3)),
            // The `{badmap, Map}` arms: the classification runs in argument order and a
            // word-identical non-map pair is not a merge (rules 1-2).
            (
                "err_same_non_map",
                invoke(reg, ctx, "maps", "merge", &[nine, nine]).is_err(),
            ),
            (
                "err_first_non_map",
                invoke(reg, ctx, "maps", "merge", &[nine, blank_empty]).is_err(),
            ),
            (
                "err_second_non_map",
                invoke(reg, ctx, "maps", "merge", &[m, nine]).is_err(),
            ),
            ("remove_absent", same_word(call(reg, ctx, "maps", "remove", &[zz, m]), m)),
            // `with/2` is the negative pin: no identity on either side.
            (
                "with_one_key",
                same_word(call(reg, ctx, "maps", "with", &[key_list, m]), m),
            ),
        ];
        for (name, got) in rows {
            let want = expected
                .iter()
                .find(|(row, _)| *row == name)
                .map(|(_, answer)| *answer)
                .unwrap_or_else(|| panic!("`{name}` is not in the expected matrix"));
            assert_eq!(got, want, "this runtime's `{name}` row");
            assert_eq!(
                oracle.get(name).copied(),
                Some(got),
                "`{name}`: the oracle and this runtime must agree"
            );
        }
    });
}
