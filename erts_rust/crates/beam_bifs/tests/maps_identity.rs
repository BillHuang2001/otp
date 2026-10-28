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
//! `maps:merge/2`: a call that provably changes nothing returns the *argument map
//! term*, not a rebuilt copy.
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
//! the test asserts the returned term is **word-identical to the argument** (a `true`
//! row) or a **different word** (a `false` row), and the content assertions keep the
//! answers honest.
//!
//! # Oracle matrix (measured on the system OTP 29 / erts-17.1, compiled module,
//! `V = 42`, `M = #{a => V}`)
//!
//! ```text
//! erts_debug:same(maps:put(a,V,M), M)                        => true  (same value word)
//! erts_debug:same(maps:put(b,1,M), M)                        => false
//! erts_debug:same(maps:put(b,1,#{a=>1,b=>2}), #{a=>1,b=>2})   => false
//! erts_debug:same(maps:put(a,43,M), M)                       => false
//! erts_debug:same(maps:update(a,V,M), M)                     => true
//! erts_debug:same(maps:merge(M,#{}), M)                      => true  (empty Map2)
//! erts_debug:same(maps:merge(#{},M), M)                      => true  (empty Map1)
//! erts_debug:same(maps:merge(M,#{a=>V}), M)                  => true  (Map2 adds nothing)
//! erts_debug:same(maps:merge(M,#{b=>1}), M)                  => false
//! erts_debug:same(maps:remove(zz,M), M)                      => true  (absent key)
//! erts_debug:same(maps:with([a],M), M)                       => false (negative pin)
//! ```
//!
//! # One documented divergence, `erts_debug:same/2`-only
//!
//! `flatmap_merge` prefers **`Map2`** when the two maps have the *same key set* with
//! identical value words (`if (n == n2) { … return map2; }`, `erl_map.c`), so for two
//! *distinct* maps with equal content the oracle answers `same(maps:merge(M1, M2), M2)`
//! ⇒ `true` and `same(maps:merge(M1, M2), M1)` ⇒ `false`. This runtime returns `Map1`
//! there (`args[0]`, the "changed nothing" answer); the two agree at `=:=` and differ
//! only under `erts_debug:same/2`. The live-oracle cross-check at the bottom of this
//! file pins both readings so the divergence cannot drift silently.

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
fn merge_hands_back_an_argument_when_it_adds_nothing() {
    run(|reg, ctx| {
        let (a, b) = (atom(ctx, "a"), atom(ctx, "b"));
        let v = int(42);
        let m = map_of(reg, ctx, &[(a, v)]);

        // An empty Map2 contributes nothing ⇒ Map1.
        let blank = empty(reg, ctx);
        let got = call(reg, ctx, "maps", "merge", &[m, blank]);
        assert!(same_word(got, m), "maps:merge/2 with an empty Map2");

        // An empty Map1 means the union *is* Map2.
        let blank = empty(reg, ctx);
        let got = call(reg, ctx, "maps", "merge", &[blank, m]);
        assert!(same_word(got, m), "maps:merge/2 with an empty Map1");

        // Map2 whose only pair is already present with the same value word ⇒ Map1.
        let echo = map_of(reg, ctx, &[(a, v)]);
        assert!(!same_word(echo, m), "the echo map is a distinct heap term");
        let got = call(reg, ctx, "maps", "merge", &[m, echo]);
        assert!(same_word(got, m), "maps:merge/2 with a redundant Map2");

        // A new key in Map2 is a real merge.
        let extra = map_of(reg, ctx, &[(b, int(1))]);
        let got = call(reg, ctx, "maps", "merge", &[m, extra]);
        assert!(!same_word(got, m), "maps:merge/2 that adds a key");
        assert_eq!(size_of(reg, ctx, got), 2);

        // So is a changed value on a shared key.
        let changed = map_of(reg, ctx, &[(a, int(43))]);
        let got = call(reg, ctx, "maps", "merge", &[m, changed]);
        assert!(!same_word(got, m), "maps:merge/2 that changes a value");
        assert!(same_word(get(reg, ctx, a, got), int(43)), "Map2 wins");
    });
}

#[test]
fn merge_compares_value_words_not_structure() {
    run(|reg, ctx| {
        let a = atom(ctx, "a");
        let elems = [int(1), int(2)];
        let first = ctx.list(&elems).expect("fits");
        let second = ctx.list(&elems).expect("fits");
        let m = map_of(reg, ctx, &[(a, first)]);

        // `Map2`'s value is `=:=` but a different word ⇒ real union (rebuild).
        let twin = map_of(reg, ctx, &[(a, second)]);
        let got = call(reg, ctx, "maps", "merge", &[m, twin]);
        assert!(!same_word(got, m), "distinct value words are a real merge");
        assert!(
            compare::eq(get(reg, ctx, a, got), second, ctx.atoms()),
            "Map2's value wins"
        );

        // The same word is the redundant case.
        let echo = map_of(reg, ctx, &[(a, first)]);
        let got = call(reg, ctx, "maps", "merge", &[m, echo]);
        assert!(same_word(got, m), "the same value word adds nothing");
    });
}

/// The one place this runtime is deliberately *more* eager than the oracle: with two
/// **distinct** maps that share their whole key set and every value word,
/// `maps:merge/2` returns `Map1` where `flatmap_merge`'s `n == n2` arm returns `Map2`.
/// `=:=` cannot see the difference; only `erts_debug:same/2` can (see the module docs,
/// and the live-oracle cross-check below, which pins both readings).
#[test]
fn merge_with_an_identical_key_set_returns_map1() {
    run(|reg, ctx| {
        let a = atom(ctx, "a");
        let v = int(42);
        let m1 = map_of(reg, ctx, &[(a, v)]);
        let m2 = map_of(reg, ctx, &[(a, v)]);
        assert!(!same_word(m1, m2), "distinct heap maps");

        let got = call(reg, ctx, "maps", "merge", &[m1, m2]);
        assert!(same_word(got, m1), "ours: Map1 (`args[0]`)");
        assert!(!same_word(got, m2), "the oracle would answer Map2 here");
        assert!(
            compare::eq(got, m2, ctx.atoms()),
            "the content is Map2's either way"
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
        // …a redundant Map2…
        let echo = map_of_ints(reg, ctx, &[(7, 7)]);
        let got = call(reg, ctx, "maps", "merge", &[hamt, echo]);
        assert!(same_word(got, hamt), "merge(HAMT, a redundant pair)");
        // …and a real one.
        let extra = map_of_ints(reg, ctx, &[(99, 99)]);
        let got = call(reg, ctx, "maps", "merge", &[hamt, extra]);
        assert!(!same_word(got, hamt), "merge(HAMT, a new key)");
        assert_eq!(size_of(reg, ctx, got), 41);
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
/// run by `erl_eval`, whose literal handling is not a compiled module's.
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
    %% A distinct map with the very same content as M (no literal sharing).
    Echo = maps:from_list([{A, Value}]),
    Rows = [
        {"put_same_value",         is(maps:put(A, Value, M), M)},
        {"put_new_key",            is(maps:put(B, 1, M), M)},
        {"put_into_two",           is(maps:put(B, 1, Two), Two)},
        {"put_new_value",          is(maps:put(A, 43, M), M)},
        {"update_same_value",      is(maps:update(A, Value, M), M)},
        {"merge_empty_second",     is(maps:merge(M, maps:new()), M)},
        {"merge_empty_first",      is(maps:merge(maps:new(), M), M)},
        {"merge_adds_key",         is(maps:merge(M, maps:put(B, 1, maps:new())), M)},
        {"merge_same_set_is_map1", is(maps:merge(M, Echo), M)},
        {"merge_same_set_is_map2", is(maps:merge(M, Echo), Echo)},
        {"remove_absent",          is(maps:remove(Z, M), M)},
        {"with_one_key",           is(maps:with([A], M), M)}
    ],
    [io:format("~s=~s~n", [Name, atom_to_list(Answer)]) || {Name, Answer} <- Rows],
    ok.

is(X, Y) -> erts_debug:same(X, Y).
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
/// runtime's own answers, which the sections above pin. Skips LOUDLY when `erlc`/`erl`
/// is unavailable (CI's `cargo test` job has no oracle).
///
/// The two `merge_same_set_*` rows are the documented divergence: the oracle returns
/// **`Map2`** for two distinct maps with an identical key set, this runtime returns
/// `Map1`. Both sides are asserted, so neither reading can drift unnoticed.
#[test]
fn the_identity_matrix_agrees_with_the_live_oracle() {
    let Some(oracle) = compiled_module_oracle() else {
        eprintln!(
            "SKIP: the system `erlc`/`erl` oracle is unavailable or does not run; \
             the maps no-op-identity cross-check is skipped"
        );
        return;
    };

    // The oracle's own answers (the matrix in the module docs).
    let expected: &[(&str, bool)] = &[
        ("put_same_value", true),
        ("put_new_key", false),
        ("put_into_two", false),
        ("put_new_value", false),
        ("update_same_value", true),
        ("merge_empty_second", true),
        ("merge_empty_first", true),
        ("merge_adds_key", false),
        ("remove_absent", true),
        ("with_one_key", false),
        // The documented `n == n2` arm: the oracle prefers Map2.
        ("merge_same_set_is_map1", false),
        ("merge_same_set_is_map2", true),
    ];
    for (name, answer) in expected {
        assert_eq!(
            oracle.get(*name).copied(),
            Some(*answer),
            "the live oracle's `{name}` row"
        );
    }

    // This runtime's answers, observed as word identity through the real registry —
    // the same rows, so the two columns are comparable.
    run(|reg, ctx| {
        let (a, b, zz) = (atom(ctx, "a"), atom(ctx, "b"), atom(ctx, "zz"));
        let v = int(42);
        let m = map_of(reg, ctx, &[(a, v)]);
        let two = map_of(reg, ctx, &[(a, int(1)), (b, int(2))]);
        let echo = map_of(reg, ctx, &[(a, v)]);
        let blank = empty(reg, ctx);
        let blank2 = empty(reg, ctx);
        let extra = map_of(reg, ctx, &[(b, int(1))]);

        let rows: Vec<(&str, bool, bool)> = vec![
            (
                "put_same_value",
                same_word(call(reg, ctx, "maps", "put", &[a, v, m]), m),
                true,
            ),
            (
                "put_new_key",
                same_word(call(reg, ctx, "maps", "put", &[b, int(1), m]), m),
                false,
            ),
            (
                "put_into_two",
                same_word(call(reg, ctx, "maps", "put", &[b, int(1), two]), two),
                false,
            ),
            (
                "put_new_value",
                same_word(call(reg, ctx, "maps", "put", &[a, int(43), m]), m),
                false,
            ),
            (
                "update_same_value",
                same_word(call(reg, ctx, "maps", "update", &[a, v, m]), m),
                true,
            ),
            (
                "merge_empty_second",
                same_word(call(reg, ctx, "maps", "merge", &[m, blank]), m),
                true,
            ),
            (
                "merge_empty_first",
                same_word(call(reg, ctx, "maps", "merge", &[blank2, m]), m),
                true,
            ),
            (
                "merge_adds_key",
                same_word(call(reg, ctx, "maps", "merge", &[m, extra]), m),
                false,
            ),
            (
                "merge_same_set_is_map1",
                same_word(call(reg, ctx, "maps", "merge", &[m, echo]), m),
                true,
            ),
            (
                "merge_same_set_is_map2",
                same_word(call(reg, ctx, "maps", "merge", &[m, echo]), echo),
                false,
            ),
            (
                "remove_absent",
                same_word(call(reg, ctx, "maps", "remove", &[zz, m]), m),
                true,
            ),
        ];
        for (name, got, want) in rows {
            assert_eq!(got, want, "this runtime's `{name}` row");
            if name.starts_with("merge_same_set_") {
                // The documented divergence: the oracle answers from Map2, we from
                // Map1. `=:=` agrees either way.
                continue;
            }
            assert_eq!(
                oracle.get(name).copied(),
                Some(got),
                "`{name}`: the oracle and this runtime must agree"
            );
        }

        // `with/2` is the negative pin: no identity on either side.
        let keys = ctx.list(&[a]).expect("fits");
        let got = same_word(call(reg, ctx, "maps", "with", &[keys, m]), m);
        assert!(!got, "maps:with/2 builds a fresh map");
        assert_eq!(oracle.get("with_one_key").copied(), Some(false), "oracle with");
    });
}
