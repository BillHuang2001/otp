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

//! Integration tests: the terminal printer (`~w`/`~p`/`~W`/`~P`, `io:format` and
//! `io_lib:format`) renders a **non-flat (HAMT) map**, not only a flat one.
//!
//! Before this fix `src/print.rs`/`src/print/pretty.rs` read a map exclusively
//! through the flat-only `flatmap_pairs`, which is `badarg` for a >32-pair hash
//! map, so `io:format("~p", [HAMTMap])` raised `badarg` where the oracle prints.
//! Both now read through [`beam_term::map::map_pairs_storage_order`], whose order
//! (`maps:keys/1`'s order) is what the oracle prints.
//!
//! Every golden below was produced by
//!
//! ```text
//! erl -noshell -eval 'io:format(Fmt, [Term]), halt().'
//! ```
//!
//! on the OTP 29.1.1 / erts-17.1 oracle. A map's printed order is the *storage*
//! order: for a flat map (≤32 pairs) that is the oracle's flatmap key order, and
//! for a hash map (>32 pairs) the HAMT slot order. Integer, binary and
//! integer-tuple keys hash from their content (`beam_term::ihash`), so their HAMT
//! order is portable and is pinned byte-for-byte here (and re-checked against a
//! live `erl` where available); **atom** keys hash from their (VM-local) atom
//! index, so that order is *not* portable across VMs — the atom-keyed case asserts
//! the storage order instead of an oracle golden.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::map::{map_pairs_storage_order, map_words_capacity, write_map};
use beam_term::prelude::*;

use std::process::Command;

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// An atom table seeded with the atoms the tests need (`k1`..`k40` for the
/// atom-keyed hash map, plus the printable-string/short-circuit fixtures).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    let names: Vec<String> = ["true", "false", "ok", "a", "b", "c", "x", "xy"]
        .iter()
        .map(|s| (*s).to_string())
        .chain((1..=40).map(|i| format!("k{i}")))
        .collect();
    atoms.seed(names.iter().map(String::as_str));
    atoms
}

/// A flat proper list of the byte-sized integers of `bytes` (an Erlang string).
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let mut elems = Vec::with_capacity(bytes.len());
    for &b in bytes {
        elems.push(ctx.int(i64::from(b)).expect("a byte fits a small int"));
    }
    ctx.list(&elems).expect("a short list fits")
}

/// A binary built through the registered `erlang:list_to_binary/1`.
fn binary(ctx: &mut BifContext<'_>, reg: &BifRegistry, bytes: &[u8]) -> Eterm {
    let list = byte_list(ctx, bytes);
    reg.call("erlang", "list_to_binary", 1, ctx, &[list])
        .expect("list_to_binary fits")
}

/// Builds a map with [`write_map`], which selects the representation the oracle
/// does: a flat map at ≤32 distinct keys, a hash map (HAMT) above.
///
/// `write_map` lays out a HAMT whose stored pair order equals the oracle's
/// `maps:to_list/1` order (`beam_term::map`).
fn write_map_of(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let cap = map_words_capacity(pairs.len());
    // SAFETY: `ctx.heap()` hands out `cap` contiguous, exclusively-owned words from a
    // `beam_alloc::Heap` that never reallocates and outlives every term derived from
    // it, which is exactly `write_map`'s reservation contract.
    let hp = unsafe { ctx.heap().alloc_words(cap) }.expect("the test heap has room");
    // SAFETY: `hp` points at `cap == map_words_capacity(pairs.len())` exclusive words.
    unsafe { write_map(hp, pairs, ctx.atoms()) }.expect("a well-formed map")
}

/// A map `#{1 => 1, 2 => 2, …, N => N}` (flat for N ≤ 32, a HAMT above).
fn integer_map(ctx: &mut BifContext<'_>, n: i64) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (1..=n)
        .map(|i| {
            let k = ctx.int(i).expect("a small int");
            (k, k)
        })
        .collect();
    write_map_of(ctx, &pairs)
}

/// A map whose keys are `<<N>>` (a binary), value `N`.
fn binary_key_map(ctx: &mut BifContext<'_>, reg: &BifRegistry, n: i64) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (1..=n)
        .map(|i| {
            let key = binary(ctx, reg, &[i as u8]);
            (key, ctx.int(i).expect("a small int"))
        })
        .collect();
    write_map_of(ctx, &pairs)
}

/// A map with mixed content-hashed keys — `{N,0}` when `N rem 3 == 0`, `<<N>>`
/// when `N rem 3 == 1`, else the integer `N` — value `N`, the oracle's
/// `case N rem 3 of 0 -> {N,0}; 1 -> <<N>>; 2 -> N end`.
fn mixed_key_map(ctx: &mut BifContext<'_>, reg: &BifRegistry, n: i64) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (1..=n)
        .map(|i| {
            let key = match i % 3 {
                0 => {
                    let k = ctx.int(i).expect("a small int");
                    let zero = ctx.int(0).expect("a small int");
                    ctx.tuple(&[k, zero]).expect("a 2-tuple fits")
                }
                1 => binary(ctx, reg, &[i as u8]),
                _ => ctx.int(i).expect("a small int"),
            };
            (key, ctx.int(i).expect("a small int"))
        })
        .collect();
    write_map_of(ctx, &pairs)
}

/// A map `#{k1 => 1, k2 => 2, …, kN => N}` with atom keys (a HAMT for N > 32).
fn atom_key_map(ctx: &mut BifContext<'_>, n: i64) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (1..=n)
        .map(|i| {
            let key = ctx.atom(&format!("k{i}")).expect("a seeded atom");
            (key, ctx.int(i).expect("a small int"))
        })
        .collect();
    write_map_of(ctx, &pairs)
}

/// Runs `io:format(Fmt, [Arg])` through a `BytesSink`; returns `(bytes, result)`.
fn run_io(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Eterm,
) -> (Vec<u8>, Result<(), BeamError>) {
    run_io_args(reg, fmt, |ctx, reg| vec![build(ctx, reg)])
}

/// Runs `io:format(Fmt, Args)` through a `BytesSink`; returns `(bytes, result)`.
fn run_io_args(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Vec<Eterm>,
) -> (Vec<u8>, Result<(), BeamError>) {
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let result;
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, fmt);
        let args = build(&mut ctx, reg);
        let args = ctx.list(&args).expect("a short arg list fits");
        result = reg
            .call("io", "format", 2, &mut ctx, &[format, args])
            .map(|_| ());
    }
    (sink.contents().to_vec(), result)
}

/// `io:format(Fmt, [Arg])` text.
fn io_text(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Eterm,
) -> String {
    let (out, result) = run_io(reg, fmt, build);
    assert!(result.is_ok(), "{fmt:?}: {result:?}");
    String::from_utf8(out).expect("the printer emits UTF-8")
}

/// `io_lib:format(Fmt, [Arg])` then `lists:flatten/1` — the directive's text, but
/// through the `io_lib` entry point (which returns a deep list).
fn io_lib_text(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Eterm,
) -> Vec<u8> {
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let format = byte_list(&mut ctx, fmt);
    let arg = build(&mut ctx, reg);
    let args = ctx.list(&[arg]).expect("a short arg list fits");
    let deep = reg
        .call("io_lib", "format", 2, &mut ctx, &[format, args])
        .expect("io_lib:format");
    let flat = reg
        .call("lists", "flatten", 1, &mut ctx, &[deep])
        .expect("lists:flatten/1");
    proper_bytes(flat)
}

/// The byte values of a proper character list (`signed_value` of each element).
fn proper_bytes(term: Eterm) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = beam_codec::term::cons_head(cur).expect("a live cons cell");
        let value = signed_value(head);
        assert!(
            (0..=255).contains(&value),
            "a character-list element out of byte range: {value}"
        );
        out.push(value as u8);
        cur = beam_codec::term::cons_tail(cur).expect("a live cons cell");
    }
    assert!(is_nil(cur), "not a proper list");
    out
}

/// Runs `io:format(Fmt, [Expr])` under the system `erl`; `None` (with a loud
/// message) when the oracle is unavailable, so the cross-check skips rather than
/// fails.
fn oracle_io(fmt: &str, expr: &str) -> Option<Vec<u8>> {
    let statement = format!("io:format(\"{fmt}\", [{expr}]), halt().");
    match Command::new("erl")
        .args(["-noshell", "-eval", &statement])
        .output()
    {
        Ok(out) if out.status.success() => Some(out.stdout),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

/// Removes the re-flow whitespace (spaces and newlines) so two renderings that
/// differ only in line-breaking can be compared for their pair order.
fn strip_layout(text: &str) -> String {
    text.chars().filter(|c| !matches!(c, ' ' | '\n')).collect()
}

// ---------------------------------------------------------------------------
// Flat maps (≤32 pairs) — the order must not change
// ---------------------------------------------------------------------------

/// A flat map keeps rendering in the oracle's flatmap key order: the removal of the
/// local `flatmap_key_cmp` re-sort must not disturb it (the storage order of a flat
/// map *is* that order). Goldens are the oracle's `io:format` text.
#[test]
fn flat_integer_key_map_renders_like_the_oracle() {
    let reg = default_registry();
    let map5 = |ctx: &mut BifContext<'_>, _: &BifRegistry| integer_map(ctx, 5);

    assert_eq!(
        io_text(&reg, b"~w", map5),
        "#{1 => 1,2 => 2,3 => 3,4 => 4,5 => 5}"
    );
    assert_eq!(
        io_text(&reg, b"~p", map5),
        "#{1 => 1,2 => 2,3 => 3,4 => 4,5 => 5}"
    );

    // `~W`/`~P` depth limiting (`#{...}` / `#{}`).
    let with_depth = |depth: i64| {
        move |ctx: &mut BifContext<'_>, _: &BifRegistry| {
            vec![integer_map(ctx, 5), ctx.int(depth).expect("fits")]
        }
    };
    for (depth, want) in [
        (1i64, "#{}"),
        (2, "#{1 => 1,...}"),
        (3, "#{1 => 1,2 => 2,...}"),
    ] {
        let (out, result) = run_io_args(&reg, b"~W", with_depth(depth));
        assert!(result.is_ok(), "~W flat {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~W flat {depth}"
        );
    }
    for (depth, want) in [(1i64, "#{...}"), (2, "#{1 => 1,...}")] {
        let (out, result) = run_io_args(&reg, b"~P", with_depth(depth));
        assert!(result.is_ok(), "~P flat {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P flat {depth}"
        );
    }

    // `io_lib:format/2` flattens to the same text.
    assert_eq!(
        io_lib_text(&reg, b"~w", map5),
        b"#{1 => 1,2 => 2,3 => 3,4 => 4,5 => 5}".to_vec()
    );
}

/// A 20-pair flat map re-flows under `~p` exactly as the oracle (the pre-existing
/// `tests/pretty_print.rs` golden, kept here so the flat re-flow is covered next to
/// the hash-map cases).
#[test]
fn flat_map_pretty_reflow_is_unchanged() {
    let reg = default_registry();
    let text = io_text(&reg, b"~p", |ctx, _| integer_map(ctx, 20));
    assert_eq!(
        text,
        "#{1 => 1,2 => 2,3 => 3,4 => 4,5 => 5,6 => 6,7 => 7,8 => 8,9 => 9,10 => 10,\n  \
         11 => 11,12 => 12,13 => 13,14 => 14,15 => 15,16 => 16,17 => 17,18 => 18,\n  \
         19 => 19,20 => 20}"
    );
}

/// A **flat** map is also rendered in its storage order, and for a canonical flat map
/// that order *is* the oracle's flatmap key order (`erts_cmp_flatmap_keys`,
/// `erl_utils.h:267`) — so removing the printer's local re-sort changes nothing.
///
/// The pairs are inserted scrambled and [`write_map`] canonicalises them exactly as the
/// oracle stores them (`maps:from_list/1`'s order), then the render is compared to the
/// live oracle. Only portable cases are used: integer keys order by value, a number
/// precedes an atom by term order, and binary keys order by content — none of these
/// depends on the VM-local atom index.
#[test]
fn flat_map_scrambled_insertion_matches_the_live_oracle() {
    let reg = default_registry();

    // `maps:from_list([{3,4},{1,2}])` ⇒ `#{1 => 2,3 => 4}` (integer keys by value).
    let Some(expected) = oracle_io("~w", "maps:from_list([{3,4},{1,2}])") else {
        return;
    };
    assert_eq!(expected, b"#{1 => 2,3 => 4}", "oracle golden");
    let (out, result) = run_io(&reg, b"~w", |ctx, _| {
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        let three = ctx.int(3).expect("fits");
        let four = ctx.int(4).expect("fits");
        write_map_of(ctx, &[(three, four), (one, two)])
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, expected, "integer-keyed flat map ~w");

    // `maps:from_list([{b,1},{1,2}])` ⇒ `#{1 => 2,b => 1}` (number before atom).
    let Some(expected) = oracle_io("~w", "maps:from_list([{b,1},{1,2}])") else {
        return;
    };
    assert_eq!(expected, b"#{1 => 2,b => 1}", "oracle golden");
    let (out, result) = run_io(&reg, b"~w", |ctx, _| {
        let b = ctx.atom("b").expect("a seeded atom");
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        write_map_of(ctx, &[(b, one), (one, two)])
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, expected, "atom/integer-keyed flat map ~w");

    // `maps:from_list([{<<3>>,4},{<<1>>,2}])` ⇒ `#{<<1>> => 2,<<3>> => 4}`.
    let Some(expected) = oracle_io("~w", "maps:from_list([{<<3>>,4},{<<1>>,2}])") else {
        return;
    };
    assert_eq!(expected, b"#{<<1>> => 2,<<3>> => 4}", "oracle golden");
    let (out, result) = run_io(&reg, b"~w", |ctx, reg| {
        let k1 = binary(ctx, reg, &[1]);
        let k3 = binary(ctx, reg, &[3]);
        let two = ctx.int(2).expect("fits");
        let four = ctx.int(4).expect("fits");
        write_map_of(ctx, &[(k3, four), (k1, two)])
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, expected, "binary-keyed flat map ~w");
}

// ---------------------------------------------------------------------------
// Hash maps (>32 pairs) — the bug this fixes
// ---------------------------------------------------------------------------
/// The 33-pair integer-keyed map is a HAMT. Every control renders the oracle's HAMT
/// traversal order (`maps:keys/1`'s order), not a `badarg` and not a sorted order.
#[test]
fn hamt_integer_key_map_renders_like_the_oracle() {
    let reg = default_registry();
    let map33 = |ctx: &mut BifContext<'_>, _: &BifRegistry| integer_map(ctx, 33);

    // `io:format("~w", [maps:from_list([{N,N} || N <- lists:seq(1,33)])])`.
    let want_w = "#{18 => 18,4 => 4,12 => 12,19 => 19,29 => 29,13 => 13,2 => 2,7 => 7,31 => 31,\
                  8 => 8,10 => 10,23 => 23,9 => 9,15 => 15,32 => 32,1 => 1,25 => 25,28 => 28,\
                  20 => 20,6 => 6,11 => 11,17 => 17,24 => 24,14 => 14,33 => 33,3 => 3,16 => 16,\
                  30 => 30,21 => 21,5 => 5,27 => 27,26 => 26,22 => 22}";
    assert_eq!(io_text(&reg, b"~w", map33), want_w);

    // `io:format("~p", …)` re-flows the same pairs at the two-space map indent.
    let want_p = "#{18 => 18,4 => 4,12 => 12,19 => 19,29 => 29,13 => 13,2 => 2,7 => 7,31 => 31,\n  \
                  8 => 8,10 => 10,23 => 23,9 => 9,15 => 15,32 => 32,1 => 1,25 => 25,28 => 28,\n  \
                  20 => 20,6 => 6,11 => 11,17 => 17,24 => 24,14 => 14,33 => 33,3 => 3,\n  \
                  16 => 16,30 => 30,21 => 21,5 => 5,27 => 27,26 => 26,22 => 22}";
    assert_eq!(io_text(&reg, b"~p", map33), want_p);

    // A single-line `~w` and a re-flowed `~p` present the *same* pairs in the same
    // order (only whitespace differs).
    assert_eq!(strip_layout(want_p), strip_layout(want_w));

    // `io_lib:format/2` routes through the same renderer.
    assert_eq!(io_lib_text(&reg, b"~w", map33), want_w.as_bytes().to_vec());

    // `~W`/`~P` depth limiting — the pairs are traversed in HAMT order, one pair per
    // depth level (`~W`/`~P` 1 ⇒ the empty/elided forms; 2 ⇒ the first pair).
    let with_depth = |depth: i64| {
        move |ctx: &mut BifContext<'_>, _: &BifRegistry| {
            vec![integer_map(ctx, 33), ctx.int(depth).expect("fits")]
        }
    };
    for (depth, want) in [
        (1i64, "#{}"),
        (2, "#{18 => 18,...}"),
        (3, "#{18 => 18,4 => 4,...}"),
    ] {
        let (out, result) = run_io_args(&reg, b"~W", with_depth(depth));
        assert!(result.is_ok(), "~W hamt {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~W hamt {depth}"
        );
    }
    for (depth, want) in [
        (1i64, "#{...}"),
        (2, "#{18 => 18,...}"),
        (3, "#{18 => 18,4 => 4,...}"),
        (4, "#{18 => 18,4 => 4,12 => 12,...}"),
    ] {
        let (out, result) = run_io_args(&reg, b"~P", with_depth(depth));
        assert!(result.is_ok(), "~P hamt {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P hamt {depth}"
        );
    }
}

/// The rendered HAMT pair order equals `maps:keys/1`'s order — the invariant the
/// oracle's print satisfies (`beam_term::map::map_pairs_storage_order` is that
/// order; `beam_bifs`'s `maps:keys/1` reads the same stored order).
#[test]
fn hamt_pair_order_is_the_storage_order() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let map = integer_map(&mut ctx, 33);

    // The 33 keys in stored order, as `maps:keys/1` would report them.
    let stored: Vec<i64> = map_pairs_storage_order(map)
        .expect("a hash map")
        .iter()
        .map(|(k, _)| signed_value(*k))
        .collect();
    assert_eq!(stored.len(), 33);
    // It is a HAMT order, not the sorted key order.
    assert_ne!(stored, (1..=33).collect::<Vec<i64>>());

    // The `~w` rendering lists the pairs in that exact order.
    let text = io_text(&reg, b"~w", |ctx, _| integer_map(ctx, 33));
    let pairs: Vec<String> = stored.iter().map(|k| format!("{k} => {k}")).collect();
    let want = format!("#{{{}}}", pairs.join(","));
    assert_eq!(text, want);
}

/// The same fix for a map nested as a tuple/list element: the inner HAMT renders in
/// its stored order and the outer container re-flows around it. The `~w` forms are
/// the `~w` map golden wrapped by `{}`/`[]`; the `~p` forms re-flow at a three-space
/// continuation indent (the oracle's, verified byte for byte).
#[test]
fn nested_hamt_map_renders_like_the_oracle() {
    let reg = default_registry();
    let tuple33 = |ctx: &mut BifContext<'_>, _: &BifRegistry| {
        let m = integer_map(ctx, 33);
        ctx.tuple(&[m]).expect("a 1-tuple fits")
    };
    let list33 = |ctx: &mut BifContext<'_>, _: &BifRegistry| {
        let m = integer_map(ctx, 33);
        ctx.list(&[m]).expect("a 1-element list fits")
    };

    // `io:format("~w", [{maps:from_list([{N,N} || N <- lists:seq(1,33)])}])` and the
    // list-wrapped form.
    let want_w = "#{18 => 18,4 => 4,12 => 12,19 => 19,29 => 29,13 => 13,2 => 2,7 => 7,31 => 31,\
                  8 => 8,10 => 10,23 => 23,9 => 9,15 => 15,32 => 32,1 => 1,25 => 25,28 => 28,\
                  20 => 20,6 => 6,11 => 11,17 => 17,24 => 24,14 => 14,33 => 33,3 => 3,16 => 16,\
                  30 => 30,21 => 21,5 => 5,27 => 27,26 => 26,22 => 22}";
    assert_eq!(io_text(&reg, b"~w", tuple33), format!("{{{want_w}}}"));
    assert_eq!(io_text(&reg, b"~w", list33), format!("[{want_w}]"));

    let want_tup_p = "{#{18 => 18,4 => 4,12 => 12,19 => 19,29 => 29,13 => 13,2 => 2,7 => 7,\
                      31 => 31,\n   \
                      8 => 8,10 => 10,23 => 23,9 => 9,15 => 15,32 => 32,1 => 1,25 => 25,\
                      28 => 28,\n   \
                      20 => 20,6 => 6,11 => 11,17 => 17,24 => 24,14 => 14,33 => 33,3 => 3,\n   \
                      16 => 16,30 => 30,21 => 21,5 => 5,27 => 27,26 => 26,22 => 22}}";
    assert_eq!(io_text(&reg, b"~p", tuple33), want_tup_p);

    let want_lst_p = "[#{18 => 18,4 => 4,12 => 12,19 => 19,29 => 29,13 => 13,2 => 2,7 => 7,\
                      31 => 31,\n   \
                      8 => 8,10 => 10,23 => 23,9 => 9,15 => 15,32 => 32,1 => 1,25 => 25,\
                      28 => 28,\n   \
                      20 => 20,6 => 6,11 => 11,17 => 17,24 => 24,14 => 14,33 => 33,3 => 3,\n   \
                      16 => 16,30 => 30,21 => 21,5 => 5,27 => 27,26 => 26,22 => 22}]";
    assert_eq!(io_text(&reg, b"~p", list33), want_lst_p);
}
// ---------------------------------------------------------------------------
// Content-hashed non-integer keys — cross-checked against a live `erl`
// ---------------------------------------------------------------------------

/// A binary-keyed 33-pair HAMT map renders byte-identically to the live oracle
/// (binary keys hash from their content, so the HAMT order is portable).
#[test]
fn hamt_binary_key_map_matches_the_live_oracle() {
    let Some(expected) = oracle_io("~w", "maps:from_list([{<<N>>, N} || N <- lists:seq(1,33)])")
    else {
        return;
    };
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~w", |ctx, reg| binary_key_map(ctx, reg, 33));
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, expected, "binary-keyed HAMT map ~w");
}

/// A mixed-key (integer / binary / integer-tuple) 33-pair HAMT map renders
/// byte-identically to the live oracle.
#[test]
fn hamt_mixed_key_map_matches_the_live_oracle() {
    let Some(expected) = oracle_io(
        "~w",
        "maps:from_list([{case N rem 3 of 0 -> {N,0}; 1 -> <<N>>; 2 -> N end, N} \
         || N <- lists:seq(1,33)])",
    ) else {
        return;
    };
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~w", |ctx, reg| mixed_key_map(ctx, reg, 33));
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, expected, "mixed-key HAMT map ~w");
}

// ---------------------------------------------------------------------------
// Atom keys — the order is VM-local, so the storage order is asserted
// ---------------------------------------------------------------------------

/// An atom-keyed 33-pair HAMT map **renders** (no `badarg`) and lists its pairs in
/// the map's storage order (`map_pairs_storage_order`, i.e. `maps:keys/1`'s order).
///
/// An atom's map hash is `ihash_mix64` of its raw word, i.e. of its **atom index**
/// (`erts/emulator/beam/erl_term_hashing.c:2301-2319`, `beam_term::ihash`), and that
/// index is VM-local — so unlike the integer/binary keys above this order cannot be
/// pinned to a cross-VM oracle golden. The storage-order assertion is the portable
/// invariant; `~w` and the re-flowed `~p` must present the same order.
#[test]
fn hamt_atom_key_map_renders_in_storage_order() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let map;
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        map = atom_key_map(&mut ctx, 33);
        let format = byte_list(&mut ctx, b"~w");
        let args = ctx.list(&[map]).expect("fits");
        reg.call("io", "format", 2, &mut ctx, &[format, args])
            .expect("a hash map with atom keys renders (no badarg)");
    }
    let text = String::from_utf8(sink.contents().to_vec()).expect("the printer emits UTF-8");

    let pairs = map_pairs_storage_order(map).expect("a hash map");
    assert_eq!(pairs.len(), 33);
    let body: Vec<String> = pairs
        .iter()
        .map(|(k, v)| {
            let name = atoms.name(atom_val(*k)).expect("a seeded atom");
            format!("{name} => {}", signed_value(*v))
        })
        .collect();
    assert_eq!(text, format!("#{{{}}}", body.join(",")));

    // The re-flowed `~p` presents the same order (only whitespace differs).
    let pretty = io_text(&reg, b"~p", |ctx, _| atom_key_map(ctx, 33));
    assert_eq!(strip_layout(&pretty), strip_layout(&text));
}
