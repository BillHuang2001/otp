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

//! Integration tests for the `erts_internal`/`erts_debug` BIFs
//! (`src/erts_internal.rs`), driven through the public [`BifRegistry::call`] over a
//! real `beam_alloc::Heap` + seeded `beam_term::AtomTable`.
//!
//! The ten keys under test are the module's whole roster:
//!
//! `erts_debug:same/2`, `erts_debug:flat_size/1`, `erts_internal:term_type/1`,
//! `erts_internal:map_to_tuple_keys/1`, `erts_internal:map_hashmap_children/1`,
//! `erts_internal:map_next/3`, `erts_internal:time_unit/0`,
//! `erts_internal:perf_counter_unit/0`, `erts_internal:binary_to_integer/2`,
//! `erts_internal:list_to_integer/2`.
//!
//! Every expected value is pinned against the system `erl` (OTP 29 / erts-17.1);
//! [`pinned_scalars_agree_with_the_live_oracle`] re-checks a handful of them against
//! `erl` at test time.
//!
//! # Two of these BIFs *return* their failure atoms
//!
//! `erts_internal:binary_to_integer/2` and `erts_internal:list_to_integer/2` never
//! raise (`big.c` ends every failure path in `BIF_RET(am_badarg)` /
//! `BIF_RET(am_big)` / `am_no_integer` / `am_not_a_list`): they **return** the atoms
//! `badarg` / `big` / `no_integer` / `not_a_list`. Verified live:
//! `try erts_internal:binary_to_integer(<<>>,10) catch C:R -> {C,R} end` ⇒ the atom
//! `badarg`, whereas the public `erlang:binary_to_integer(<<>>,10)` wrapper raises
//! `error:badarg`. The assertions below therefore expect `Ok(<atom>)` for those.
//! `map_next/3`, `map_to_tuple_keys/1` and `map_hashmap_children/1` **do** raise
//! (`badarg`, or the structured `{badmap, Term}`), so those assert on `Err(..)`.
//!
//! # Representation-dependent goldens
//!
//! `erts_debug:flat_size/1` and `erts_internal:term_type/1` report the argument's
//! *physical* representation, so their values are only comparable where this runtime
//! and the C build the same block. This runtime materializes small binaries as
//! on-heap `ErlHeapBits`, exactly as the oracle does for a **dynamically built**
//! binary (e.g. `list_to_binary/1`); a *compiled literal* binary in the oracle
//! instead becomes the 8-word off-heap `[BinRef][ErlSubBits]` pair, so it reports
//! `flat_size` 8 and `term_type` `sub_binary` there. The goldens below are the
//! on-heap ones (`flat_size` `<<>>`→2, `<<1,2,3>>`→3 and `term_type`→`heap_binary`),
//! which is what `main`'s representation produces; the oracle cross-check only uses
//! *dynamically built* binaries so the comparison stays apples-to-apples.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::{PortId, ProcessId, RefAllocator};
use beam_term::prelude::{make_small, signed_value};
use beam_term::{atom_val, is_atom, is_small, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The atom names the ten BIFs and the expected-value builders need.
///
/// `a`/`b` are seeded before one another so the flat-map writer (which keeps the
/// caller's pair order verbatim, `beam_codec::term::make_flatmap`) stores
/// `#{a => 1, b => 2}` as the pair order `[a, b]` the goldens assume.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        // Booleans and the returned failure atoms.
        "true",
        "false",
        "badarg",
        "big",
        "no_integer",
        "not_a_list",
        // `map_next/3`'s mode/step atoms and the test keys.
        "iterator",
        "none",
        "acc",
        "zz",
        "notamap",
        "banana",
        "a",
        "b",
        "x",
        // The `term_type/1` result names.
        "fixnum",
        "atom",
        "nil",
        "pid",
        "port",
        "list",
        "tuple",
        "hfloat",
        "bignum",
        "flatmap",
        "hashmap",
        "heap_binary",
        "sub_binary",
        "fun",
        "export",
        "reference",
        "external_reference",
        "external_pid",
        "external_port",
        // The `badmap` structured-reason tag; the fun MFA assembly atoms.
        "badmap",
        "erlang",
        "map",
    ]);
    atoms
}

/// Builds a fresh heap/atom table and a context over them, then runs `f`.
///
/// The heap and the atom table outlive the context, so the context borrows them for
/// the closure's whole lifetime.
fn with_ctx<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let reg = default_registry();
    let mut heap = Heap::new(16384).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx)
}

/// Calls a BIF through the registry, panicking with its key on a failure.
fn call(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Eterm {
    bifs.call(module, function, arity, ctx, args)
        .unwrap_or_else(|err| panic!("{module}:{function}/{arity} raised {err:?}"))
}

/// `<<Bytes>>` built on the context heap.
fn binary(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    beam_codec::term::make_binary(ctx.heap(), bytes).expect("the binary fits")
}

/// The Erlang string `s` (a proper list of byte-sized small integers).
fn chars(ctx: &mut BifContext<'_>, s: &str) -> Eterm {
    let elems: Vec<Eterm> = s.bytes().map(|b| make_small(i64::from(b))).collect();
    ctx.list(&elems).expect("the string fits")
}

/// `#{a => 1, b => 2}`, stored in the pair order `[a, b]`.
fn ab_map(ctx: &mut BifContext<'_>) -> Eterm {
    let (a, b) = (ctx.atom("a").expect("seeded"), ctx.atom("b").expect("seeded"));
    beam_codec::term::make_flatmap(ctx.heap(), &[(a, make_small(1)), (b, make_small(2))])
        .expect("the flat map fits")
}

/// The key/value tuple `{k, v}` built on the context heap.
fn pair(ctx: &mut BifContext<'_>, k: Eterm, v: Eterm) -> Eterm {
    ctx.tuple(&[k, v]).expect("the pair fits")
}

/// An internal-reference term (the M1 ref thing), the runtime's `reference`.
fn internal_ref(heap: &mut Heap) -> Eterm {
    RefAllocator::new()
        .next()
        .to_eterm(heap)
        .expect("the ref thing fits")
}

/// A `FunKind::Local` (`make_fun3`-shaped) fun term with an empty environment.
///
/// `beam_codec::term::make_fun_local` is `#[cfg(test)]`-gated *inside* `beam_codec`
/// (unreachable from an integration test) and the production builder lives in
/// `beam_engine::terms`, which is not a dependency of this crate — so this helper
/// calls the frozen heap-agnostic writer itself. That writer is `unsafe` (it writes
/// raw heap words), which is why this is the file's only `unsafe` use.
fn local_fun(heap: &mut Heap, arity: usize, uniq: u64, index: u32) -> Eterm {
    let words = beam_codec::term::fun_words(beam_codec::term::FunKind::Local, 0);
    // SAFETY: `alloc_words` returns `words` exclusively-owned, writable words, and
    // `words` is exactly `fun_words(FunKind::Local, 0)` — the block size
    // `write_fun_local` writes.
    let hp = unsafe { heap.alloc_words(words) }.expect("the fun block fits the test heap");
    // SAFETY: `hp` owns exactly `fun_words(Local, 0)` words and the empty environment
    // satisfies `num_free == env.len()`; the block stays live on `heap` for the
    // returned term.
    unsafe { beam_term::fun::write_fun_local(hp, arity, uniq, index, &[]) }
        .expect("writes the local fun")
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// Structural term equality (`==` on a boxed `Eterm` compares by address).
fn assert_eq_term(atoms: &AtomTable, got: Eterm, expected: Eterm, what: &str) {
    assert!(
        beam_term::compare::eq(got, expected, atoms),
        "{what}: the returned term is not the expected one"
    );
}

/// Asserts the call raised a plain `badarg` (`BeamError::BadArg`).
fn assert_badarg(result: Result<Eterm>, what: &str) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected BadArg, got {other:?}"),
    }
}

/// Asserts the call raised a typed `unsupported_bif`.
fn assert_unsupported(result: Result<Eterm>, what: &str) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, ExceptionClass::Error, "{what}");
            assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"), "{what}");
        }
        other => panic!("{what}: expected unsupported_bif, got {other:?}"),
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

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[test]
fn the_registry_registers_all_ten_keys() {
    let reg = default_registry();
    for (module, name, arity) in [
        ("erts_debug", "same", 2),
        ("erts_debug", "flat_size", 1),
        ("erts_internal", "term_type", 1),
        ("erts_internal", "map_to_tuple_keys", 1),
        ("erts_internal", "map_hashmap_children", 1),
        ("erts_internal", "map_next", 3),
        ("erts_internal", "time_unit", 0),
        ("erts_internal", "perf_counter_unit", 0),
        ("erts_internal", "binary_to_integer", 2),
        ("erts_internal", "list_to_integer", 2),
    ] {
        assert!(
            reg.contains(module, name, arity),
            "missing {module}:{name}/{arity}"
        );
        assert!(
            reg.lookup(module, name, arity).is_some(),
            "no impl for {module}:{name}/{arity}"
        );
    }

    // The whole `erts_internal`/`erts_debug` namespace this crate's registry exposes is
    // exactly **91** keys: the eleven core keys above (ten registered by
    // `src/erts_internal.rs` + `erts_internal:cmp_term/2`, owned by the sibling
    // `src/compare.rs` — the term-order primitive the `<`/`=<` family routes through),
    // plus the 80 seam-backed runtime-family keys (`src/runtime/erts_internal.rs` 62 +
    // `src/runtime/erts_debug.rs` 18), each of which answers the typed `unsupported_bif`
    // with no runtime hook installed. Pinning the namespace count — rather than only
    // checking each key with `contains` — catches a stray duplicate registration.
    let erts_keys: Vec<_> = reg
        .names()
        .into_iter()
        .filter(|(m, _, _)| m == "erts_internal" || m == "erts_debug")
        .collect();
    assert_eq!(erts_keys.len(), 91, "got {erts_keys:?}");
    assert!(
        erts_keys.contains(&("erts_internal".to_string(), "cmp_term".to_string(), 2)),
        "erts_internal:cmp_term/2 (owned by compare.rs) must be the eleventh key"
    );
}

// ---------------------------------------------------------------------------
// erts_debug:same/2 — raw word identity
// ---------------------------------------------------------------------------

#[test]
fn same_is_raw_word_identity() {
    with_ctx(|reg, ctx| {
        let t = ctx.boolean(true).expect("seeded");
        let f = ctx.boolean(false).expect("seeded");
        let same = |ctx: &mut BifContext<'_>, a: Eterm, b: Eterm| {
            call(reg, ctx, "erts_debug", "same", 2, &[a, b])
        };

        // Immediates are their own word: equal immediates (and `NIL`) are `same`.
        assert_eq!(same(ctx, make_small(1), make_small(1)), t);
        assert_eq!(same(ctx, NIL, NIL), t);
        let atom = ctx.atom("a").expect("seeded");
        assert_eq!(same(ctx, atom, atom), t);
        let pid = ProcessId::new(1, 0).to_eterm();
        assert_eq!(same(ctx, pid, pid), t);

        // A small int and a float of the same numeric value are different words.
        let float_one = ctx.make_float(1.0).expect("fits");
        assert_eq!(same(ctx, make_small(1), float_one), f);

        // The *same* boxed object is `same/2`; two structurally equal cells are not.
        // (The oracle also answers `true` for two literals of one module — the
        // loader's literal-area dedup makes them one object. That is a property of
        // the literal representation, never of this BIF, which compares the two
        // `Eterm` words and nothing else.)
        let one_cons = ctx.cons(make_small(1), NIL).expect("fits");
        assert_eq!(same(ctx, one_cons, one_cons), t);
        let another_cons = ctx.cons(make_small(1), NIL).expect("fits");
        assert_eq!(same(ctx, one_cons, another_cons), f);

        let ref_a = internal_ref(ctx.heap());
        let ref_b = internal_ref(ctx.heap());
        assert_eq!(same(ctx, ref_a, ref_a), t);
        assert_eq!(same(ctx, ref_a, ref_b), f);
    });
}

// ---------------------------------------------------------------------------
// erts_debug:flat_size/1 — size_object/1
// ---------------------------------------------------------------------------

#[test]
fn flat_size_counts_immediates_and_containers() {
    with_ctx(|reg, ctx| {
        let atom = ctx.atom("a").expect("seeded");
        let float = ctx.make_float(1.5).expect("fits");
        // Two limbs: `[header, d0, d1]` = 3 words (`erase_debug:flat_size`
        // promotes `size_object` past the small-int range only for the *result*).
        let big = beam_codec::term::make_big(ctx.heap(), false, &[0x1234_5678_9abc_def0, 1])
            .expect("the bignum fits");
        let tuple3 = ctx
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("fits");
        let empty_tuple = ctx.tuple(&[]).expect("fits");
        let list3 = ctx
            .list(&[make_small(1), make_small(2), make_small(3)])
            .expect("fits");
        let inner_tuple = ctx
            .tuple(&[make_small(1), make_small(2)])
            .expect("fits");
        let inner_list = ctx
            .list(&[make_small(3), make_small(4)])
            .expect("fits");
        let nested = ctx
            .list(&[inner_tuple, inner_list])
            .expect("fits");
        let empty_map = beam_codec::term::make_flatmap(ctx.heap(), &[]).expect("fits");
        let one_map =
            beam_codec::term::make_flatmap(ctx.heap(), &[(atom, make_small(1))]).expect("fits");
        let pid = ProcessId::new(1, 0).to_eterm();

        let cases: [(&str, Eterm, i64); 12] = [
            ("small int", make_small(1), 0),
            ("atom", atom, 0),
            ("nil", NIL, 0),
            ("self()", pid, 0),
            ("float", float, 2),
            ("bignum", big, 3),
            ("{1,2,3}", tuple3, 4),
            ("{}", empty_tuple, 0),
            ("[1,2,3]", list3, 6),
            ("[{1,2},[3,4]]", nested, 11),
            ("#{}", empty_map, 3),
            ("#{a=>1}", one_map, 6),
        ];
        for (what, term, expected) in cases {
            let got = call(reg, ctx, "erts_debug", "flat_size", 1, &[term]);
            assert_eq!(got, make_small(expected), "flat_size/1 of {what}");
        }
    });
}

#[test]
fn flat_size_of_bitstrings_matches_the_on_heap_layout() {
    with_ctx(|reg, ctx| {
        // On-heap `ErlHeapBits` is `[header, size, data…]`, so
        // `heap_bits_size_words(bits) == 2 + ceil(bits / 64)` — the C's
        // `heap_bits_size` for the same on-heap block (see the module docs of
        // `src/erts_internal.rs`). The oracle builds exactly this block for a
        // dynamically constructed ≤64-byte binary (`list_to_binary/1`).
        let empty = binary(ctx, b"");
        let three = binary(ctx, &[1, 2, 3]);
        let three_bits = beam_codec::term::make_bitstring(ctx.heap(), &[0b0010_0000], 3)
            .expect("<<1:3>> fits");
        let sixty_four_bytes = binary(ctx, &[0u8; 64]);

        for (what, term, expected) in [
            ("<<>>", empty, 2),
            ("<<1,2,3>>", three, 3),
            ("<<1:3>>", three_bits, 3),
            ("<<0:512>>", sixty_four_bytes, 10),
        ] {
            let got = call(reg, ctx, "erts_debug", "flat_size", 1, &[term]);
            assert_eq!(got, make_small(expected), "flat_size/1 of {what}");
        }
    });
}

#[test]
fn flat_size_reports_a_typed_gap_for_divergent_representations() {
    with_ctx(|reg, ctx| {
        // A fun: this runtime inlines the descriptor, so the block is
        // `1 + desc_words(kind) + num_free` where the C's `ERL_FUN_SIZE + num_free`
        // is 2 + num_free — no comparable oracle value exists.
        let fun = local_fun(ctx.heap(), 0, 1, 2);
        assert_unsupported(
            reg.call("erts_debug", "flat_size", 1, ctx, &[fun]),
            "flat_size/1 of a fun",
        );

        // A bitstring above C's on-heap limit (512 bits): this runtime may hold it
        // in an on-heap `ErlHeapBits` (11 words for 520 bits) where C switches to
        // the 8-word `[BinRef][ErlSubBits]` pair.
        let long = beam_codec::term::make_bitstring(ctx.heap(), &[0u8; 65], 520)
            .expect("the 520-bit block fits");
        assert_unsupported(
            reg.call("erts_debug", "flat_size", 1, ctx, &[long]),
            "flat_size/1 of a 520-bit bitstring",
        );

        // An internal reference: the M1 ref thing is 4 words where C's
        // `ERL_REF_SIZE` is 3.
        let reference = internal_ref(ctx.heap());
        assert_unsupported(
            reg.call("erts_debug", "flat_size", 1, ctx, &[reference]),
            "flat_size/1 of an internal reference",
        );
    });
}

// ---------------------------------------------------------------------------
// erts_internal:term_type/1 — the physical tag name
// ---------------------------------------------------------------------------

#[test]
fn term_type_names_every_representation() {
    with_ctx(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let module = ctx.atoms().lookup("erlang").expect("seeded");
        let function = ctx.atoms().lookup("map").expect("seeded");
        let creator = beam_codec::term::FunPid {
            node: module,
            number: 1,
            serial: 0,
            creation: 0,
        };

        let cases: Vec<(&str, Eterm)> = vec![
            ("fixnum", make_small(1)),
            (
                "bignum",
                beam_codec::term::make_big(ctx.heap(), false, &[1, 2, 3]).expect("fits"),
            ),
            ("hfloat", ctx.make_float(1.5).expect("fits")),
            ("atom", a),
            ("nil", NIL),
            // A cons cell (proper list) and an improper list are both `list`.
            ("list", ctx.cons(make_small(1), NIL).expect("fits")),
            ("list", ctx.cons(make_small(1), make_small(2)).expect("fits")),
            (
                "tuple",
                ctx.tuple(&[make_small(1), make_small(2)]).expect("fits"),
            ),
            ("tuple", ctx.tuple(&[]).expect("fits")),
            (
                "flatmap",
                beam_codec::term::make_flatmap(ctx.heap(), &[(a, make_small(1))]).expect("fits"),
            ),
            // Any bitstring this runtime builds on the heap is `heap_binary`.
            ("heap_binary", binary(ctx, b"abc")),
            (
                "heap_binary",
                beam_codec::term::make_bitstring(ctx.heap(), &[0b0010_0000], 3).expect("fits"),
            ),
            ("heap_binary", binary(ctx, b"")),
            ("pid", ProcessId::new(1, 0).to_eterm()),
            ("reference", internal_ref(ctx.heap())),
            ("fun", local_fun(ctx.heap(), 0, 1, 2)),
            (
                "export",
                beam_codec::term::make_fun_external(ctx.heap(), 2, module, function)
                    .expect("fits"),
            ),
            (
                "export",
                beam_codec::term::make_fun_imported(
                    ctx.heap(),
                    1,
                    module,
                    &[7u8; 16],
                    3,
                    4,
                    5,
                    creator,
                    &[],
                )
                .expect("fits"),
            ),
            ("port", PortId::new(1).to_eterm()),
        ];

        for (name, term) in cases {
            let expected = ctx.atom(name).unwrap_or_else(|_| {
                panic!("`{name}` is not seeded — the atom table would make the BIF fail")
            });
            let got = call(reg, ctx, "erts_internal", "term_type", 1, &[term]);
            assert_eq!(got, expected, "term_type/1 => {name}");
        }
    });
}

// ---------------------------------------------------------------------------
// erts_internal:map_to_tuple_keys/1
// ---------------------------------------------------------------------------

#[test]
fn map_to_tuple_keys_returns_the_key_tuple() {
    with_ctx(|reg, ctx| {
        let (a, b) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
        );
        // `#{a => 1, b => 2}` -> `{a, b}` in the map's stored order.
        let map = ab_map(ctx);
        let keys = call(reg, ctx, "erts_internal", "map_to_tuple_keys", 1, &[map]);
        let expected = ctx.tuple(&[a, b]).expect("fits");
        assert_eq_term(ctx.atoms(), keys, expected, "map_to_tuple_keys(#{a=>1,b=>2})");

        // `#{}` -> `{}`.
        let empty = beam_codec::term::make_flatmap(ctx.heap(), &[]).expect("fits");
        let keys = call(reg, ctx, "erts_internal", "map_to_tuple_keys", 1, &[empty]);
        let expected = ctx.tuple(&[]).expect("fits");
        assert_eq_term(ctx.atoms(), keys, expected, "map_to_tuple_keys(#{})");
    });
}

#[test]
fn map_to_tuple_keys_of_a_non_map_raises_badmap() {
    with_ctx(|reg, ctx| {
        let result = reg.call("erts_internal", "map_to_tuple_keys", 1, ctx, &[make_small(5)]);
        let reason = structured_reason(result, "map_to_tuple_keys/1 of 5");
        // The live reason term is a real `{badmap, 5}` tuple (a boxed term compares
        // by address, so compare structurally).
        let tag = ctx.atom("badmap").expect("seeded");
        let expected = ctx.tuple(&[tag, make_small(5)]).expect("fits");
        assert_eq_term(
            ctx.atoms(),
            reason,
            expected,
            "map_to_tuple_keys/1 of 5 reason",
        );
    });
}

// ---------------------------------------------------------------------------
// erts_internal:map_hashmap_children/1
// ---------------------------------------------------------------------------

#[test]
fn map_hashmap_children_of_a_flat_map_raises_badarg() {
    with_ctx(|reg, ctx| {
        // `HAMT_SUBTAG_HEAD_FLATMAP` => the C raises a plain `badarg`.
        let map = ab_map(ctx);
        assert_badarg(
            reg.call("erts_internal", "map_hashmap_children", 1, ctx, &[map]),
            "map_hashmap_children/1 of a flat map",
        );
    });
}

#[test]
fn map_hashmap_children_of_a_non_map_raises_badmap() {
    with_ctx(|reg, ctx| {
        let result = reg.call("erts_internal", "map_hashmap_children", 1, ctx, &[make_small(5)]);
        let reason = structured_reason(result, "map_hashmap_children/1 of 5");
        let tag = ctx.atom("badmap").expect("seeded");
        let expected = ctx.tuple(&[tag, make_small(5)]).expect("fits");
        assert_eq_term(
            ctx.atoms(),
            reason,
            expected,
            "map_hashmap_children/1 of 5 reason",
        );
    });
}

// ---------------------------------------------------------------------------
// erts_internal:map_next/3
// ---------------------------------------------------------------------------

#[test]
fn map_next_folds_a_flat_map_in_list_mode() {
    with_ctx(|reg, ctx| {
        let map = ab_map(ctx);
        let (a, b) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
        );
        let acc = ctx.atom("acc").expect("seeded");
        let p1 = pair(ctx, a, make_small(1));
        let p2 = pair(ctx, b, make_small(2));
        let two_pairs = ctx.list(&[p1, p2]).expect("fits");
        // The caller's accumulator is the *mode* argument itself; the fold prepends the
        // map's pairs onto it verbatim. `single_acc` is `[acc]`, so folding the two
        // pairs onto it yields `[{a,1},{b,2},acc]` (= `with_acc`). Oracle-pinned:
        // `erts_internal:map_next(0, #{a=>1,b=>2}, [acc])` and the `1` variant both
        // return `[{a,1},{b,2},acc]` (the position only bounds-checks `n >= Path`).
        let single_acc = ctx.list(&[acc]).expect("fits");
        let with_acc = ctx.list(&[p1, p2, acc]).expect("fits");

        for (what, path, mode, expected) in [
            ("(0, M, [])", make_small(0), NIL, two_pairs),
            ("(2, M, [])", make_small(2), NIL, two_pairs),
            ("(0, M, [acc])", make_small(0), single_acc, with_acc),
            ("(1, M, [acc])", make_small(1), single_acc, with_acc),
        ] {
            let got = call(reg, ctx, "erts_internal", "map_next", 3, &[path, map, mode]);
            assert_eq_term(ctx.atoms(), got, expected, what);
        }

        // `(0, #{}, [])` is the empty accumulator.
        let empty = beam_codec::term::make_flatmap(ctx.heap(), &[]).expect("fits");
        let got = call(reg, ctx, "erts_internal", "map_next", 3, &[make_small(0), empty, NIL]);
        assert_eq!(got, NIL, "(0, #{{}}, [])");
    });
}

#[test]
fn map_next_folds_a_flat_map_in_iterator_mode() {
    with_ctx(|reg, ctx| {
        let map = ab_map(ctx);
        let (a, b) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
        );
        let iterator = ctx.atom("iterator").expect("seeded");
        let none = ctx.atom("none").expect("seeded");
        let p1 = pair(ctx, a, make_small(1));
        let p2 = pair(ctx, b, make_small(2));

        // `(0, M, iterator)` / `(1, M, iterator)` fold every pair into
        // `{K0, V0, {K1, V1, …, none}}`.
        let tail = ctx.tuple(&[b, make_small(2), none]).expect("fits");
        let expected = ctx.tuple(&[a, make_small(1), tail]).expect("fits");
        for what in ["(0, M, iterator)", "(1, M, iterator)"] {
            let path = if what.starts_with("(0") { make_small(0) } else { make_small(1) };
            let got = call(reg, ctx, "erts_internal", "map_next", 3, &[path, map, iterator]);
            assert_eq_term(ctx.atoms(), got, expected, what);
        }

        // `(0, #{}, iterator)` is `none`.
        let empty = beam_codec::term::make_flatmap(ctx.heap(), &[]).expect("fits");
        let got = call(reg, ctx, "erts_internal", "map_next", 3, &[make_small(0), empty, iterator]);
        assert_eq!(got, none, "(0, #{{}}, iterator)");

        let _ = (p1, p2);
    });
}

#[test]
fn map_next_walks_the_key_list_of_an_ordered_iterator() {
    with_ctx(|reg, ctx| {
        let map = ab_map(ctx);
        let (a, b) = (
            ctx.atom("a").expect("seeded"),
            ctx.atom("b").expect("seeded"),
        );
        let iterator = ctx.atom("iterator").expect("seeded");
        let none = ctx.atom("none").expect("seeded");

        let ab = ctx.list(&[a, b]).expect("fits");
        let ba = ctx.list(&[b, a]).expect("fits");
        let just_a = ctx.list(&[a]).expect("fits");
        let zz = ctx.atom("zz").expect("seeded");
        let just_zz = ctx.list(&[zz]).expect("fits");

        let ab_tail = ctx.tuple(&[b, make_small(2), none]).expect("fits");
        let expect_ab = ctx.tuple(&[a, make_small(1), ab_tail]).expect("fits");
        let ba_tail = ctx.tuple(&[a, make_small(1), none]).expect("fits");
        let expect_ba = ctx.tuple(&[b, make_small(2), ba_tail]).expect("fits");
        let expect_a = ctx
            .tuple(&[a, make_small(1), none])
            .expect("fits");

        // `([a,b], M, iterator)` — every pair, in the path's order.
        let got = call(reg, ctx, "erts_internal", "map_next", 3, &[ab, map, iterator]);
        assert_eq_term(ctx.atoms(), got, expect_ab, "([a,b], M, iterator)");

        // `([b,a], M, iterator)` — the path's order decides.
        let got = call(reg, ctx, "erts_internal", "map_next", 3, &[ba, map, iterator]);
        assert_eq_term(ctx.atoms(), got, expect_ba, "([b,a], M, iterator)");

        // `([a], M, iterator)` — one step then `none`.
        let got = call(reg, ctx, "erts_internal", "map_next", 3, &[just_a, map, iterator]);
        assert_eq_term(ctx.atoms(), got, expect_a, "([a], M, iterator)");

        // An empty path is `none` (both for a non-empty and an empty map).
        let got = call(reg, ctx, "erts_internal", "map_next", 3, &[NIL, map, iterator]);
        assert_eq!(got, none, "([], M, iterator)");
        let empty = beam_codec::term::make_flatmap(ctx.heap(), &[]).expect("fits");
        let got = call(reg, ctx, "erts_internal", "map_next", 3, &[NIL, empty, iterator]);
        assert_eq!(got, none, "([], #{{}}, iterator)");

        // A path key the map does not hold is `badarg`.
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[just_zz, map, iterator]),
            "([zz], M, iterator)",
        );
    });
}

#[test]
fn map_next_rejects_bad_arguments_with_badarg() {
    with_ctx(|reg, ctx| {
        let map = ab_map(ctx);
        let iterator = ctx.atom("iterator").expect("seeded");
        let banana = ctx.atom("banana").expect("seeded");
        let notamap = ctx.atom("notamap").expect("seeded");
        let just_a = ctx.list(&[ctx.atom("a").expect("seeded")]).expect("fits");

        // A non-map is a plain `badarg` here (never `{badmap, _}`, `erl_map.c:3644`).
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[make_small(0), notamap, NIL]),
            "(0, notamap, [])",
        );
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[make_small(0), notamap, iterator]),
            "(0, notamap, iterator)",
        );
        // A mode that is neither `iterator` nor a list/NIL.
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[make_small(0), map, banana]),
            "(0, M, banana)",
        );
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[make_small(0), map, make_small(5)]),
            "(0, M, 5)",
        );
        // A list path is only valid in iterator mode.
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[just_a, map, NIL]),
            "([a], M, [])",
        );
        // A non-integer position.
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[banana, map, NIL]),
            "(banana, M, [])",
        );
        // A negative position: 60-bit two's complement makes its unsigned value huge,
        // so `n >= unsigned_val(Path)` fails.
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[make_small(-1), map, iterator]),
            "(-1, M, iterator)",
        );
        // `n < Path` (the map holds 2 pairs).
        assert_badarg(
            reg.call("erts_internal", "map_next", 3, ctx, &[make_small(3), map, iterator]),
            "(3, M, iterator)",
        );
    });
}

// ---------------------------------------------------------------------------
// erts_internal:time_unit/0 and perf_counter_unit/0
// ---------------------------------------------------------------------------

#[test]
fn time_units_are_nanoseconds() {
    with_ctx(|reg, ctx| {
        // This runtime fixes the native time unit to nanoseconds (`src/time.rs`), as
        // on the Linux host the goldens were probed on.
        let nanos = make_small(1_000_000_000);
        assert_eq!(
            call(reg, ctx, "erts_internal", "time_unit", 0, &[]),
            nanos,
            "erts_internal:time_unit/0"
        );
        assert_eq!(
            call(reg, ctx, "erts_internal", "perf_counter_unit", 0, &[]),
            nanos,
            "erts_internal:perf_counter_unit/0"
        );
    });
}

// ---------------------------------------------------------------------------
// erts_internal:binary_to_integer/2 — returns its failure atoms
// ---------------------------------------------------------------------------

/// A 40-digit decimal — more digits than `digits_per_small(10) == 17`.
const FORTY_DIGITS: &str = "1234567890123456789012345678901234567890";

#[test]
fn binary_to_integer_parses_and_returns_its_failure_atoms() {
    with_ctx(|reg, ctx| {
        let ten = make_small(10);
        let sixteen = make_small(16);
        let badarg = ctx.atom("badarg").expect("seeded");
        let big = ctx.atom("big").expect("seeded");

        // Successes (a small integer), with the optional sign and leading zeroes.
        for (input, base, expected) in [
            ("123", 10, 123),
            ("-42", 10, -42),
            ("+42", 10, 42),
            ("0000", 10, 0),
            ("007", 10, 7),
            ("ff", 16, 255),
        ] {
            let arg = binary(ctx, input.as_bytes());
            let base = if base == 10 { ten } else { sixteen };
            let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[arg, base]);
            assert_eq!(got, make_small(expected), "binary_to_integer({input}, {base:?})");
        }

        // The failure *atoms* (`binary_to_integer/2` never raises). Note that a
        // well-formed but over-long input is the `big` atom, not `badarg` — those are
        // asserted separately below.
        for (what, arg, base) in [
            ("<<>>", binary(ctx, b""), ten),
            ("\"1a\"", binary(ctx, b"1a"), ten),
            ("\"-\"", binary(ctx, b"-"), ten),
            ("\" 12\" (leading space)", binary(ctx, b" 12"), ten),
        ] {
            let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[arg, base]);
            assert_eq!(got, badarg, "binary_to_integer({what})");
        }

        // A 20-hex-digit value is refused by the digit-count pre-check, so it is the
        // `big` atom — not a wrongly truncated small integer.
        let arg = binary(ctx, b"FFFFFFFFFFFFFFFFFFFF");
        let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[arg, sixteen]);
        assert_eq!(got, big, "binary_to_integer(\"FFFF…\", 16)");

        // A 40-digit decimal is `big` too.
        let arg = binary(ctx, FORTY_DIGITS.as_bytes());
        let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[arg, ten]);
        assert_eq!(got, big, "binary_to_integer({FORTY_DIGITS}, 10)");

        // An out-of-range or non-integer base is `badarg`.
        for (what, base) in [
            ("base 1", make_small(1)),
            ("base 37", make_small(37)),
            ("base 0", make_small(0)),
            ("base -1", make_small(-1)),
            ("base foo", ctx.atom("x").expect("seeded")),
        ] {
            let arg = binary(ctx, b"123");
            let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[arg, base]);
            assert_eq!(got, badarg, "binary_to_integer(\"123\", {what})");
        }

        // A non-byte-aligned bitstring and a non-binary first argument are `badarg`.
        let unaligned = beam_codec::term::make_bitstring(ctx.heap(), &[0b0010_0000], 3)
            .expect("<<1:3>> fits");
        let got = call(
            reg,
            ctx,
            "erts_internal",
            "binary_to_integer",
            2,
            &[unaligned, ten],
        );
        assert_eq!(got, badarg, "binary_to_integer(<<1:3>>, 10)");

        let list_arg = ctx.list(&[make_small(49)]).expect("fits");
        let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[list_arg, ten]);
        assert_eq!(got, badarg, "binary_to_integer([49], 10)");
    });
}

// ---------------------------------------------------------------------------
// The letter-digit bound of the shared digit predicate
// ---------------------------------------------------------------------------

/// The letter-digit arms of `c2int_is_valid_char` (`big.c:4065-4072`): the highest
/// valid letter *digit* is `'A' + (base - 11)`, so the C's exclusive upper bound is
/// `'A' + base - 10`. `"ff"` parses in base 16 only because that bound is inclusive
/// of `'F'`; a `- 11` typo silently rejects the last digit of every base (`F`/`f` in
/// base 16, `Z`/`z` in base 36, …). Both parsers route through this predicate, so the
/// boundary is pinned here (oracle-probed on erts-17.1).
#[test]
fn letter_digits_parse_up_to_the_oracle_base_bound() {
    with_ctx(|reg, ctx| {
        let badarg = ctx.atom("badarg").expect("seeded");
        let sixteen = make_small(16);

        // Base 16: `F`/`f` are the last valid digits (15); `G`/`g` are not.
        for (input, expected) in [("F", 15), ("f", 15), ("ff", 255)] {
            let arg = binary(ctx, input.as_bytes());
            let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[arg, sixteen]);
            assert_eq!(got, make_small(expected), "binary_to_integer({input:?}, 16)");
        }
        for input in ["G", "g", "1g"] {
            let arg = binary(ctx, input.as_bytes());
            let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[arg, sixteen]);
            assert_eq!(got, badarg, "binary_to_integer({input:?}, 16)");
        }

        // Base 36: `Z`/`z` are the last valid digits (35).
        for input in ["Z", "z"] {
            let arg = binary(ctx, input.as_bytes());
            let got = call(reg, ctx, "erts_internal", "binary_to_integer", 2, &[arg, make_small(36)]);
            assert_eq!(got, make_small(35), "binary_to_integer({input:?}, 36)");
        }

        // The list parser shares the predicate: `"ff"` base 16 ⇒ `{255, []}` and the
        // leading `'g'` of `"1g"` stays unconsumed (⇒ `{1, "g"}`).
        let ff = chars(ctx, "ff");
        let got = call(reg, ctx, "erts_internal", "list_to_integer", 2, &[ff, sixteen]);
        let empty = chars(ctx, "");
        let expected = ctx.tuple(&[make_small(255), empty]).expect("fits");
        assert_eq_term(ctx.atoms(), got, expected, "list_to_integer(\"ff\", 16)");

        let one_g = chars(ctx, "1g");
        let got = call(reg, ctx, "erts_internal", "list_to_integer", 2, &[one_g, sixteen]);
        let g = chars(ctx, "g");
        let expected = ctx.tuple(&[make_small(1), g]).expect("fits");
        assert_eq_term(ctx.atoms(), got, expected, "list_to_integer(\"1g\", 16)");
    });
}

// ---------------------------------------------------------------------------
// erts_internal:list_to_integer/2 — returns its failure atoms / {Integer, Rest}
// ---------------------------------------------------------------------------

#[test]
fn list_to_integer_returns_the_integer_and_the_rest_of_the_list() {
    with_ctx(|reg, ctx| {
        let ten = make_small(10);
        let sixteen = make_small(16);

        // `{Integer, Rest}` — the parse stops at the first invalid digit.
        for (input, base_num, value, rest) in [
            ("123", 10, 123, ""),
            ("123abc", 10, 123, "abc"),
            ("00", 10, 0, ""),
            ("00x", 10, 0, "x"),
            ("007", 10, 7, ""),
            ("-42", 10, -42, ""),
            ("+42", 10, 42, ""),
            ("-42x", 10, -42, "x"),
            ("ff", 16, 255, ""),
            ("1z", 16, 1, "z"),
        ] {
            let list = chars(ctx, input);
            let base = if base_num == 10 { ten } else { sixteen };
            let got = call(reg, ctx, "erts_internal", "list_to_integer", 2, &[list, base]);
            let expected_int = ctx.int(value).expect("fits");
            let rest = chars(ctx, rest);
            let expected = ctx.tuple(&[expected_int, rest]).expect("fits");
            assert_eq_term(
                ctx.atoms(),
                got,
                expected,
                &format!("list_to_integer({input:?}, {base_num})"),
            );
        }

        // The failure atoms.
        let no_integer = ctx.atom("no_integer").expect("seeded");
        let not_a_list = ctx.atom("not_a_list").expect("seeded");
        let badarg = ctx.atom("badarg").expect("seeded");
        let big = ctx.atom("big").expect("seeded");

        for (what, list) in [
            ("\"abc\"", chars(ctx, "abc")),
            ("\"\"", NIL),
            ("\"-\"", chars(ctx, "-")),
            ("\"-x\"", chars(ctx, "-x")),
            // 300 is a `small` whose low byte (44 = `,`) is not a base-10 digit.
            ("[300,50]", ctx.list(&[make_small(300), make_small(50)]).expect("fits")),
        ] {
            let got = call(reg, ctx, "erts_internal", "list_to_integer", 2, &[list, ten]);
            assert_eq!(got, no_integer, "list_to_integer({what}, 10)");
        }

        // An improper list is parsed up to its non-list tail.
        let improper = ctx.cons(make_small(49), make_small(50)).expect("fits");
        let got = call(reg, ctx, "erts_internal", "list_to_integer", 2, &[improper, ten]);
        let expected = ctx.tuple(&[make_small(1), make_small(50)]).expect("fits");
        assert_eq_term(ctx.atoms(), got, expected, "list_to_integer([49|50], 10)");

        // A non-list first argument.
        let got = call(reg, ctx, "erts_internal", "list_to_integer", 2, &[make_small(5), ten]);
        assert_eq!(got, not_a_list, "list_to_integer(5, 10)");

        // An out-of-range or non-integer base.
        for (what, base) in [
            ("base 1", make_small(1)),
            ("base 37", make_small(37)),
            ("base foo", ctx.atom("x").expect("seeded")),
        ] {
            let list = chars(ctx, "123");
            let got = call(reg, ctx, "erts_internal", "list_to_integer", 2, &[list, base]);
            assert_eq!(got, badarg, "list_to_integer(\"123\", {what})");
        }

        // An over-long decimal is the `big` atom.
        let list = chars(ctx, FORTY_DIGITS);
        let got = call(reg, ctx, "erts_internal", "list_to_integer", 2, &[list, ten]);
        assert_eq!(got, big, "list_to_integer({FORTY_DIGITS}, 10)");
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

/// Evaluates one fully-qualified Erl expression with the system `erl` and returns
/// its trimmed `io:format("~p", [..])` rendering, or `None` if `erl` could not be
/// run or failed.
fn oracle_eval(expr: &str) -> Option<String> {
    let code = format!("io:format(\"~p~n\",[{expr}]), halt().");
    let output = std::process::Command::new("erl")
        .arg("-noshell")
        .arg("-eval")
        .arg(&code)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The `~p` rendering of a *scalar* term (a small integer or an atom).
fn scalar_render(atoms: &AtomTable, term: Eterm) -> String {
    if is_small(term) {
        signed_value(term).to_string()
    } else if is_atom(term) {
        atoms
            .name(atom_val(term))
            .expect("the atom is interned")
            .to_string()
    } else {
        panic!("scalar_render: only small integers and atoms are scalars")
    }
}

/// A producer of the Rust-side value for one oracle expression.
type Producer = fn(&BifRegistry, &mut BifContext<'_>) -> Eterm;

/// Cross-checks a handful of scalar results against the live system `erl`.
///
/// `erts_debug:flat_size` is exercised with **dynamically built** binaries
/// (`list_to_binary/1`), which the oracle keeps in the same on-heap `ErlHeapBits`
/// this runtime materializes; a *compiled literal* binary would be the off-heap
/// 8-word pair there and is therefore not comparable (see the module docs).
/// Skips loudly when `erl` is unavailable.
#[test]
fn pinned_scalars_agree_with_the_live_oracle() {
    if !erl_available() {
        eprintln!(
            "SKIP: the system `erl` oracle is unavailable or does not run; \
             the erts_internal/erts_debug scalar cross-check is skipped"
        );
        return;
    }

    let cases: &[(&str, Producer)] = &[
        ("erts_internal:time_unit()", |b, c| {
            call(b, c, "erts_internal", "time_unit", 0, &[])
        }),
        ("erts_internal:perf_counter_unit()", |b, c| {
            call(b, c, "erts_internal", "perf_counter_unit", 0, &[])
        }),
        ("erts_debug:flat_size(1)", |b, c| {
            call(b, c, "erts_debug", "flat_size", 1, &[make_small(1)])
        }),
        ("erts_debug:flat_size([1,2,3])", |b, c| {
            let list = c
                .list(&[make_small(1), make_small(2), make_small(3)])
                .expect("fits");
            call(b, c, "erts_debug", "flat_size", 1, &[list])
        }),
        ("erts_debug:flat_size({1,2,3})", |b, c| {
            let tuple = c
                .tuple(&[make_small(1), make_small(2), make_small(3)])
                .expect("fits");
            call(b, c, "erts_debug", "flat_size", 1, &[tuple])
        }),
        ("erts_debug:flat_size(list_to_binary([1,2,3]))", |b, c| {
            let bin = binary(c, &[1, 2, 3]);
            call(b, c, "erts_debug", "flat_size", 1, &[bin])
        }),
        (
            "erts_debug:flat_size(list_to_binary(lists:duplicate(64,0)))",
            |b, c| {
                let bin = binary(c, &[0u8; 64]);
                call(b, c, "erts_debug", "flat_size", 1, &[bin])
            },
        ),
        ("erts_internal:term_type(1)", |b, c| {
            call(b, c, "erts_internal", "term_type", 1, &[make_small(1)])
        }),
        ("erts_internal:term_type(a)", |b, c| {
            let a = c.atom("a").expect("seeded");
            call(b, c, "erts_internal", "term_type", 1, &[a])
        }),
        ("erts_internal:binary_to_integer(<<\"123\">>,10)", |b, c| {
            let bin = binary(c, b"123");
            call(
                b,
                c,
                "erts_internal",
                "binary_to_integer",
                2,
                &[bin, make_small(10)],
            )
        }),
        ("erts_internal:binary_to_integer(<<\"1a\">>,10)", |b, c| {
            let bin = binary(c, b"1a");
            call(
                b,
                c,
                "erts_internal",
                "binary_to_integer",
                2,
                &[bin, make_small(10)],
            )
        }),
        ("erts_internal:list_to_integer(\"abc\",10)", |b, c| {
            let list = chars(c, "abc");
            call(
                b,
                c,
                "erts_internal",
                "list_to_integer",
                2,
                &[list, make_small(10)],
            )
        }),
    ];

    let reg = default_registry();
    let mut heap = Heap::new(16384).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for &(expr, produce) in cases {
        let expected = oracle_eval(expr)
            .unwrap_or_else(|| panic!("the system `erl` failed to evaluate `{expr}`"));
        let got = produce(&reg, &mut ctx);
        let rendered = scalar_render(&atoms, got);
        assert_eq!(rendered, expected, "mismatch against the oracle for `{expr}`");
    }
}
