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

//! HAMT (hash-map) pair-**storage order** cross-check against the **real system
//! Erlang** (`erl`, OTP 29 / erts-17.1).
//!
//! This is the crate-local oracle slice for the [`write_map`] hash-map placement:
//! a map built with more than [`MAP_SMALL_MAP_LIMIT`] distinct keys is a HAMT keyed
//! by [`erts_map_hash`](crate::ihash::erts_map_hash), and the *order* in which
//! [`map_pairs_storage_order`] walks that HAMT is exactly the order
//! `maps:to_list/1` prints under the oracle. Reproducing that order is what
//! unblocks a byte-exact ETF image of a >32-key map for the `beam_codec` owner
//! (the serializer must emit an existing map's pairs in the map's own stored order
//! rather than re-sorting them).
//!
//! # What is pinned
//!
//! * **Two literal vectors** — reproduced bit-exactly and asserted as literal
//!   expected lists, so the check still has value when `erl` is absent:
//!   * 60 small-integer keys `#{K => K*2}` (`K = 1..=60`);
//!   * 40 binary keys `#{<<N>> => N}` (`N = 1..=40`).
//! * **Live-oracle cases** for 33/40/50/64/100 keys with a *mix* of small-integer,
//!   binary and atom keys, comparing [`map_pairs_storage_order`] against
//!   `maps:to_list/1` of the equivalent map under the oracle.
//! * A **flat map** (≤32 keys) is checked as an *observation* only: a flat map's
//!   stored key order is `erts_cmp_flatmap_keys` order (atoms by index, everything
//!   else by term), which is *already* what the oracle stores and what
//!   [`crate::compare`] / [`crate::hashing`] depend on, so it is **not** changed by
//!   this work.
//!
//! # Atom indices
//!
//! `erts_map_hash/1` hashes an **immediate** term from its raw `Eterm` word, so an
//! atom key's HAMT slot depends on the atom's *table index*. The oracle's indices
//! for the pre-defined atoms are reproduced here by seeding the [`AtomTable`] from
//! the runtime's own `atom.names` (`../../../../erts/emulator/beam/atom.names`),
//! which is the same source ERTS initializes its atom table from; the atom keys
//! are therefore placed identically on both sides. (This is an inherent property of
//! the C hash — its own comment "Limitation #1" notes the internal hash is not
//! portable between VM instances.)
//!
//! # Protocol / absence of the oracle
//!
//! Like `tests/oracle.rs`, the check talks to `erl` over a tiny **text protocol**
//! (no ETF re-implementation here — the crate cannot dev-depend on `beam_codec`
//! without a dependency cycle): the oracle is spawned with
//! `-noshell -eval '<expr>, halt(0).'`. If `erl` cannot be spawned / does not answer
//! within [`ERL_TIMEOUT`] the affected checks **skip loudly** and return, never
//! failing the suite.

use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_term::prelude::*;

/// Wall-clock budget for a single `erl` invocation before the oracle is declared
/// unusable and the affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// A word written around every term buffer to detect any overflow of the reserved
/// capacity: `write_map` must never touch it.
const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF);

/// The runtime's canonical atom list, the source ERTS initializes its atom table
/// from. Seeding [`AtomTable`] from it reproduces the oracle's atom indices.
const ATOM_NAMES: &str = include_str!("../../../../erts/emulator/beam/atom.names");

/// Bare (unquoted) pre-defined atom names used as atom keys. Their indices are
/// their position in [`ATOM_NAMES`], which the oracle reproduces at boot.
const ATOM_POOL: &[&str] = &[
    "infinity",
    "timeout",
    "normal",
    "call",
    "return",
    "throw",
    "error",
    "exit",
    "undefined",
    "nocatch",
    "undefined_function",
    "undefined_lambda",
    "nil",
    "no",
    "none",
    "abandoned",
    "abort",
    "abs_path",
    "ac",
    "access",
    "active",
    "active_tasks",
    "active_tasks_all",
    "alias",
    "alive",
    "all",
    "all_but_first",
    "all_names",
    "alloc_info",
    "alloc_sizes",
    "allocated",
    "allocated_areas",
    "allocator",
    "allocator_sizes",
    "alloc_util_allocators",
    "allow_passive_connect",
    "already_exists",
    "already_loaded",
    "anchored",
    "andalso",
];

// ---------------------------------------------------------------------------
// Oracle driver (text protocol) — mirrors tests/oracle.rs
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or
/// `None` (after a clear skip message) when `erl` is unusable.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        // The receiver may have timed out and gone away; that is fine.
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "`erl` exited with {} for expression:\n{code}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Parses an Erlang list of integers printed by `~w`, e.g. `[54,18,4]`.
fn parse_int_list(s: &str) -> Vec<i64> {
    let s = s.trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or_else(|| panic!("expected an Erlang list, got {s:?}"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|tok| {
            tok.trim()
                .parse::<i64>()
                .unwrap_or_else(|e| panic!("expected an integer, got {:?}: {e}", tok.trim()))
        })
        .collect()
}

/// The ordinal sequence of a `maps:from_list`-built map under the oracle:
/// the map is built with each key mapped to its own 0-based ordinal, then
/// `maps:to_list/1` is walked and the ordinals are printed. At least one key
/// ordinal is asserted so an accidental empty/garbage oracle answer cannot pass
/// silently.
fn oracle_ordinal_order(map_literal: &str, n_keys: usize) -> Option<Vec<i64>> {
    let expr = format!(
        "M = {map_literal}, io:format(\"~w\", [[I || {{_,I}} <- maps:to_list(M)]])"
    );
    let out = erl_stdout(&expr)?;
    let order = parse_int_list(&out);
    assert_eq!(
        order.len(),
        n_keys,
        "oracle returned {} ordinals for a {n_keys}-key map: {out:?}",
        order.len(),
    );
    Some(order)
}

// ---------------------------------------------------------------------------
// Term builders
// ---------------------------------------------------------------------------

/// Builds a map from `pairs` into a sentinel-guarded buffer and returns the buffer
/// (which owns the map's storage) plus the tagged map root.
fn build_map(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> (Vec<Eterm>, Eterm) {
    let cap = map_words_capacity(pairs.len());
    let mut buf = vec![SENTINEL; cap + 64];
    // SAFETY: `buf` owns `cap + 64 >= map_words_capacity(pairs.len())` words;
    // `write_map` writes at most `map_words_capacity` of them, and the buffer is
    // never reallocated before the term is used.
    let map = unsafe { write_map(buf.as_mut_ptr(), pairs, atoms) }.expect("write_map");
    for (i, w) in buf[cap..].iter().enumerate() {
        assert_eq!(
            *w,
            SENTINEL,
            "write_map overflowed map_words_capacity by {} words",
            i + 1,
        );
    }
    (buf, map)
}

/// Builds an on-heap bitstring (a binary when `bytes.len() * 8` bits) into its own
/// owned buffer, kept alive by `store`. The buffer is heap-allocated and never
/// moved, so the returned term's interior pointer stays valid while `store` lives.
fn build_binary_key(store: &mut Vec<Vec<Eterm>>, bytes: &[u8]) -> Eterm {
    let n_bits = bytes.len() * 8;
    let words = heap_bits_size_words(n_bits);
    let mut buf = vec![SENTINEL; words];
    // SAFETY: `buf` owns exactly `heap_bits_size_words(n_bits)` writable words.
    let t = unsafe { write_heap_bits(buf.as_mut_ptr(), bytes, n_bits) }.expect("write_heap_bits");
    store.push(buf);
    t
}

/// Maps each `(key, value)` pair of a map to its value ordinal, in the map's
/// stored order (`value == make_small(ordinal)` by construction).
fn stored_ordinals(map: Eterm) -> Vec<i64> {
    map_pairs_storage_order(map)
        .expect("map_pairs_storage_order")
        .into_iter()
        .map(|(_k, v)| signed_value(v))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 60 small-integer keys `#{K => K*2}` (the first verified oracle vector): the
/// stored key order and `maps:to_list/1` order are identical.
#[test]
fn small_int_vector_matches_the_oracle() {
    const EXPECTED: [i64; 60] = [
        54, 18, 4, 34, 46, 52, 12, 19, 41, 29, 13, 42, 55, 2, 7, 38, 56, 31, 8, 44, 10, 23, 9, 49,
        15, 35, 36, 32, 58, 1, 25, 28, 60, 20, 6, 43, 57, 11, 48, 17, 24, 37, 40, 14, 33, 53, 3,
        16, 30, 21, 5, 27, 47, 26, 51, 22, 50, 45, 59, 39,
    ];
    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = (1..=60i64)
        .map(|k| (make_small(k), make_small(k * 2)))
        .collect();
    let (_buf, map) = build_map(&pairs, &atoms);
    // SAFETY: `map` is a live boxed map with 60 keys.
    assert!(unsafe { is_hashmap(map) }, "60 keys must be a hash map");

    // Literal expectation (works without `erl`).
    let got: Vec<i64> = map_pairs_storage_order(map)
        .unwrap()
        .into_iter()
        .map(|(k, _)| signed_value(k))
        .collect();
    assert_eq!(got, EXPECTED, "stored key order != the verified oracle vector");

    // Live-oracle cross-check.
    let expr = "M = maps:from_list([{K,K*2} || K <- lists:seq(1,60)]), \
                io:format(\"~w\", [maps:keys(M)])";
    let Some(out) = erl_stdout(expr) else {
        return;
    };
    let oracle = parse_int_list(&out);
    assert_eq!(oracle, EXPECTED, "the oracle disagrees with the vector");
    assert_eq!(got, oracle, "stored key order != maps:keys order");
}

/// 40 binary keys `#{<<N>> => N}` (the second verified oracle vector).
#[test]
fn binary_vector_matches_the_oracle() {
    const EXPECTED: [i64; 40] = [
        23, 35, 11, 17, 31, 16, 7, 29, 33, 36, 18, 3, 39, 24, 38, 20, 27, 6, 14, 12, 22, 40, 21,
        10, 8, 1, 28, 30, 9, 5, 2, 32, 15, 34, 13, 25, 19, 37, 26, 4,
    ];
    let atoms = AtomTable::new();
    let mut store: Vec<Vec<Eterm>> = Vec::new();
    let pairs: Vec<(Eterm, Eterm)> = (1..=40u8)
        .map(|n| (build_binary_key(&mut store, &[n]), make_small(n as i64)))
        .collect();
    let (_buf, map) = build_map(&pairs, &atoms);
    // SAFETY: `map` is a live boxed map with 40 keys.
    assert!(unsafe { is_hashmap(map) }, "40 keys must be a hash map");

    // Literal expectation (works without `erl`).
    let got = stored_ordinals(map);
    assert_eq!(got, EXPECTED, "stored N order != the verified oracle vector");

    // Live-oracle cross-check.
    let expr = "M = maps:from_list([{<<N>>,N} || N <- lists:seq(1,40)]), \
                io:format(\"~w\", [[N || {<<N>>,_} <- maps:to_list(M)]])";
    let Some(out) = erl_stdout(expr) else {
        return;
    };
    let oracle = parse_int_list(&out);
    assert_eq!(oracle, EXPECTED, "the oracle disagrees with the vector");
    assert_eq!(got, oracle, "stored N order != maps:to_list order");
}

/// Builds a mixed atom/small-integer/binary map of `n` keys (key `i` mapped to its
/// ordinal `i`) plus the equivalent Erlang literal. Boxed binary keys are stored in
/// `store`, which the caller keeps alive.
fn mixed_map(n: usize, atoms: &AtomTable, store: &mut Vec<Vec<Eterm>>) -> (Vec<(Eterm, Eterm)>, String) {
    let mut pairs = Vec::with_capacity(n);
    let mut literals = Vec::with_capacity(n);
    for i in 0..n {
        let (key, lit) = match i % 3 {
            0 => (make_small(i as i64), format!("{i}")),
            1 => {
                let b = i as u8;
                (build_binary_key(store, &[b]), format!("<<{b}>>"))
            }
            _ => {
                let name = ATOM_POOL[i / 3];
                let atom = atoms.lookup(name).unwrap_or_else(|| panic!("atom {name} not interned"));
                (make_atom(atom), name.to_string())
            }
        };
        pairs.push((key, make_small(i as i64)));
        literals.push(format!("{{{lit},{i}}}"));
    }
    (pairs, format!("maps:from_list([{}])", literals.join(",")))
}

/// The stored order of a mixed >32-key map equals `maps:to_list/1` under the
/// oracle, for 33/40/50/64/100 keys.
#[test]
fn mixed_hash_maps_match_the_oracle() {
    let atoms = AtomTable::from_atom_names(ATOM_NAMES).expect("parse atom.names");
    for n in [33usize, 40, 50, 64, 100] {
        let mut store: Vec<Vec<Eterm>> = Vec::new();
        let (pairs, literal) = mixed_map(n, &atoms, &mut store);
        let (_buf, map) = build_map(&pairs, &atoms);
        // SAFETY: `map` is a live boxed map with `n > 32` keys.
        assert!(unsafe { is_hashmap(map) }, "n={n} must be a hash map");
        assert_eq!(map_size(map, &atoms).unwrap(), n, "n={n}");

        // Every key is retrievable (a lookup uses the same hash path).
        for (k, _) in &pairs {
            assert!(
                map_get(map, *k, &atoms).unwrap().is_some(),
                "n={n}: map_get missed a key",
            );
        }

        let got = stored_ordinals(map);

        let Some(oracle) = oracle_ordinal_order(&literal, n) else {
            return;
        };
        assert_eq!(got, oracle, "n={n}: stored order != maps:to_list order");

        // The check has teeth: a >32-key HAMT never stores its pairs in insertion
        // order, so a buggy placement (or an oracle that silently answered with the
        // input list) would be caught here.
        let insertion: Vec<i64> = (0..n as i64).collect();
        assert_ne!(oracle, insertion, "n={n}: oracle returned the insertion order");
        assert_ne!(got, insertion, "n={n}: stored order equals the insertion order");
    }
}

/// The stored order is a deterministic function of the pair *set*, independent of
/// the caller's insertion order, for a >32-key mixed map.
#[test]
fn mixed_storage_order_is_insertion_order_free() {
    let atoms = AtomTable::from_atom_names(ATOM_NAMES).expect("parse atom.names");
    let mut store: Vec<Vec<Eterm>> = Vec::new();
    let (pairs, _literal) = mixed_map(64, &atoms, &mut store);
    let (_b1, m1) = build_map(&pairs, &atoms);

    let mut reversed = pairs.clone();
    reversed.reverse();
    let (_b2, m2) = build_map(&reversed, &atoms);

    assert_eq!(stored_ordinals(m1), stored_ordinals(m2));
}

/// A flat map (≤ [`MAP_SMALL_MAP_LIMIT`] keys) is an **observation**: its stored
/// key order is `erts_cmp_flatmap_keys` order and matches the oracle already.
#[test]
fn flat_map_storage_order_is_an_observation() {
    let atoms = AtomTable::new();
    let mut store: Vec<Vec<Eterm>> = Vec::new();
    // 16 ints + 16 binaries = 32 keys ≤ the limit. Each key gets a **distinct**
    // ordinal value (the binaries' ordinals start at 16) so the ordinal sequence
    // identifies the key order unambiguously.
    let mut pairs: Vec<(Eterm, Eterm)> = Vec::new();
    let mut literals: Vec<String> = Vec::new();
    // Insert the binaries first so the stored (sorted) order differs from the
    // insertion order of the *ordinals*.
    for i in 0..16u8 {
        let ord = 16 + i as i64;
        pairs.push((build_binary_key(&mut store, &[i]), make_small(ord)));
        literals.push(format!("{{<<{i}>>,{ord}}}"));
    }
    for i in 0..16i64 {
        pairs.push((make_small(i), make_small(i)));
        literals.push(format!("{{{i},{i}}}"));
    }
    let (_buf, map) = build_map(&pairs, &atoms);
    // SAFETY: `map` is a live boxed flat map.
    assert!(unsafe { is_flatmap(map) }, "32 keys must be a flat map");

    let got = stored_ordinals(map);
    let literal = format!("maps:from_list([{}])", literals.join(","));
    let Some(oracle) = oracle_ordinal_order(&literal, 32) else {
        return;
    };
    // Observation: a flat map's stored order is `cmp_flatmap_keys` order (numbers
    // before binaries, then by term), which is representation-independent and
    // already shared with `compare`/`hashing`. It agrees with the oracle.
    assert_eq!(got, oracle, "flat-map stored order observation");
    // The observation is the key-sorted order (0..=31), not the insertion order of
    // the ordinals (16..=31 followed by 0..=15).
    let sorted: Vec<i64> = (0..32).collect();
    assert_eq!(got, sorted, "flat-map order is key-sorted");
}