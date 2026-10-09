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

//! Oracle cross-check: `beam_term`'s map and bitstring layouts / observable
//! semantics against the **real system Erlang** (`erl`, OTP 29 / erts-17.1).
//!
//! This is the crate-local slice of the workspace *compatibility gate*
//! (`../tools/differential` owns the full harness). It exercises the two interior
//! layouts `beam_term` owns — the Erlang map (flat map + HAMT) and the bitstring
//! (`ErlHeapBits`) — and checks that the **observable** results of the Rust terms
//! agree with what the oracle runtime reports for the equivalent Erlang terms:
//!
//! * map size (`maps:size`), the sorted key list (`lists:sort(maps:keys/1)`) and a
//!   set of `maps:get` results, including the flat-map / hash-map boundary at
//!   [`MAP_SMALL_MAP_LIMIT`] (31/32/33 keys) and a 40-key hash map;
//! * the flat-map **stored key order** (`cmp_flatmap_keys`): binary keys order
//!   byte-lexicographically, atom keys by atom-table **index** (not by name),
//!   cross-checked against `maps:to_list/1`;
//! * bitstring bit size (`bit_size/1`), the individual bits (`[X || <<X:1>> <= B]`)
//!   and the full bytes (`[Y || <<Y:8>> <= B]`) for `<<1:3>>`, `<<11:4>>`, a
//!   10-bit window, an arbitrary byte-aligned binary, and the empty bitstring;
//! * bignum equality / order (`=:=` / `<` / `>`), the arithmetic mixed
//!   bignum / float comparison, and the object size (`erts_debug:flat_size/1`) for
//!   positive and negative multi-limb [`write_big`] terms across the 2-/3-limb
//!   boundary (the cases that expose the header-arity convention: the header arity
//!   **is** the digit count, `big.h:70`).
//!
//! # Protocol
//!
//! Rather than re-implementing the External Term Format here (this crate cannot
//! dev-depend on `beam_codec` — that would close a dependency cycle), the check
//! talks to `erl` over a tiny **text protocol**: the oracle is spawned with
//! `-noshell -eval '<expr>, halt(0).'` and prints the observables with `io:format`.
//! The Rust side parses that text and compares it against the values read out of
//! the equivalent `beam_term` term. The two sides are built from the *same* input
//! (the same pair set / the same byte payload), so agreement is a genuine
//! oracle agreement and not a tautology.
//!
//! # Absence of the oracle is not a failure
//!
//! If `erl` is not on `PATH` (or cannot be spawned / does not answer within
//! [`ERL_TIMEOUT`]) the affected checks **skip loudly** (a clear `eprintln!`) and
//! return, never failing the suite. `cargo test -p beam_term` therefore stays
//! green on hosts without a system Erlang.

use std::cmp::Ordering;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_term::prelude::*;

/// Wall-clock budget for a single `erl` invocation before the oracle is declared
/// unusable and the affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// A word written around every term buffer to detect any overflow of the reserved
/// capacity: `write_map` / `write_heap_bits` must never touch it.
const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF);

// ---------------------------------------------------------------------------
// The oracle driver (text protocol)
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout.
///
/// Returns `None` — after printing a clear skip message — when `erl` cannot be
/// spawned or does not answer within [`ERL_TIMEOUT`]. A *spawned* `erl` that exits
/// non-zero is a bug in the embedded expression, so that is asserted rather than
/// skipped.
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

/// Parses an integer printed by `~w`.
fn parse_int(s: &str) -> i64 {
    s.trim()
        .parse::<i64>()
        .unwrap_or_else(|e| panic!("expected an integer, got {:?}: {e}", s.trim()))
}

/// Parses a `true`/`false` printed by `~w` on a boolean.
fn parse_bool(s: &str) -> bool {
    match s.trim() {
        "true" => true,
        "false" => false,
        other => panic!("expected an oracle boolean, got {other:?}"),
    }
}

/// Parses an Erlang list printed with `~w` of integers and the atom `none` (used
/// for an absent map key), e.g. `[10,20,none]`, into `Option<i64>`s.
fn parse_int_list(s: &str) -> Vec<Option<i64>> {
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
            let tok = tok.trim();
            if tok == "none" {
                None
            } else {
                Some(parse_int(tok))
            }
        })
        .collect()
}

/// Splits an oracle `a|b|...` line into its trimmed fields.
fn split_fields(out: &str) -> Vec<&str> {
    out.trim().split('|').collect()
}

// ---------------------------------------------------------------------------
// Term builders (the Rust side of every comparison)
// ---------------------------------------------------------------------------

/// Builds a map from `pairs` into a sentinel-guarded buffer and returns the buffer
/// plus the tagged map root. The buffer owns the term's storage and must stay
/// alive while `map` is used.
fn build_map(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> (Vec<Eterm>, Eterm) {
    let cap = map_words_capacity(pairs.len());
    let mut buf = vec![SENTINEL; cap + 64];
    // SAFETY: `buf` owns `cap + 64 >= map_words_capacity(pairs.len())` words;
    // `write_map` writes at most `map_words_capacity` of them, and the buffer is
    // moved (never reallocated) before the term is used.
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

/// Builds the integer-keyed map `#{1 => 10, 2 => 20, ..., n => n*10}`.
fn build_int_map(n: usize, atoms: &AtomTable) -> (Vec<Eterm>, Eterm) {
    let pairs: Vec<(Eterm, Eterm)> = (1..=n)
        .map(|i| (make_small(i as i64), make_small((i * 10) as i64)))
        .collect();
    build_map(&pairs, atoms)
}

/// The Erlang literal of the integer-keyed map `#{1 => 10, ..., n => n*10}`, as
/// `maps:from_list([{1,10},...])`.
fn erl_int_map_literal(n: usize) -> String {
    let pairs: Vec<String> = (1..=n)
        .map(|i| format!("{{{},{}}}", i, i * 10))
        .collect();
    format!("maps:from_list([{}])", pairs.join(","))
}

/// Builds an on-heap bitstring of `n_bits` bits from `bytes` into a sentinel-guarded
/// buffer.
fn build_heap_bits(bytes: &[u8], n_bits: usize) -> (Vec<Eterm>, Eterm) {
    let words = heap_bits_size_words(n_bits);
    let mut buf = vec![SENTINEL; words + 8];
    // SAFETY: `buf` owns `words + 8 >= heap_bits_size_words(n_bits)` words.
    let t = unsafe { write_heap_bits(buf.as_mut_ptr(), bytes, n_bits) }.expect("write_heap_bits");
    for (i, w) in buf[words..].iter().enumerate() {
        assert_eq!(
            *w,
            SENTINEL,
            "write_heap_bits overflowed heap_bits_size_words by {} words",
            i + 1,
        );
    }
    (buf, t)
}

/// Builds a bignum from `digits` (least-significant-first `u64` limbs) into a
/// sentinel-guarded buffer and returns the buffer plus the tagged root.
fn build_big(negative: bool, digits: &[u64]) -> (Vec<Eterm>, Eterm) {
    assert!(!digits.is_empty(), "a bignum needs at least one limb");
    let words = big_size_words(digits.len());
    let mut buf = vec![SENTINEL; words + 8];
    // SAFETY: `buf` owns `words + 8 >= big_size_words(digits.len())` words;
    // `write_big` writes exactly `big_size_words(digits.len())` of them.
    let t = unsafe { write_big(buf.as_mut_ptr(), negative, digits) };
    for (i, w) in buf[words..].iter().enumerate() {
        assert_eq!(
            *w,
            SENTINEL,
            "write_big overflowed big_size_words by {} words",
            i + 1,
        );
    }
    (buf, t)
}

/// Builds a boxed float into a sentinel-guarded buffer.
fn build_float(value: f64) -> (Vec<Eterm>, Eterm) {
    let mut buf = vec![SENTINEL; FLOAT_SIZE_WORDS + 8];
    // SAFETY: `buf` owns `FLOAT_SIZE_WORDS + 8 >= FLOAT_SIZE_WORDS` words.
    let t = unsafe { write_float(buf.as_mut_ptr(), value) };
    (buf, t)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn oracle_reports_otp_version() {
    let expr = "io:format(\"OTP ~s / erts ~s\", \
                [erlang:system_info(otp_release), erlang:system_info(version)])";
    let Some(out) = erl_stdout(expr) else {
        return;
    };
    eprintln!("beam_term oracle: system {out}");
    assert!(out.contains('/'), "unexpected version output: {out:?}");
}

/// `maps:size` and the sorted key list agree with the oracle across the flat-map /
/// hash-map boundary (31/32/33 keys) and well beyond it.
#[test]
fn oracle_agrees_on_map_size_and_keys() {
    let atoms = AtomTable::new();
    for n in [0usize, 1, 31, 32, 33, 40, 100] {
        let (_buf, map) = build_int_map(n, &atoms);

        // Internal structural expectation: `<= MAP_SMALL_MAP_LIMIT` pairs is a flat
        // map, more is a hash map. This is not observable to the oracle, but it pins
        // that the boundary case *is* exercised.
        // SAFETY: `map` is a live boxed map.
        let is_flat = unsafe { is_flatmap(map) };
        assert_eq!(
            is_flat,
            n <= MAP_SMALL_MAP_LIMIT,
            "n={n}: flat={is_flat}, limit={MAP_SMALL_MAP_LIMIT}",
        );
        assert_eq!(map_size(map, &atoms).unwrap(), n, "beam_term map_size, n={n}");

        let expr = format!(
            "M = {}, io:format(\"~w|~w\", [maps:size(M), lists:sort(maps:keys(M))])",
            erl_int_map_literal(n),
        );
        let Some(out) = erl_stdout(&expr) else {
            return;
        };
        let fields = split_fields(&out);
        assert_eq!(fields.len(), 2, "unexpected oracle output: {out:?}");

        assert_eq!(parse_int(fields[0]), n as i64, "oracle maps:size, n={n}");
        let expected_keys: Vec<Option<i64>> = (1..=n as i64).map(Some).collect();
        assert_eq!(
            parse_int_list(fields[1]),
            expected_keys,
            "oracle maps:keys, n={n}",
        );
    }
}

/// A 40-key **hash map**: `maps:get` for every present key, for several absent
/// keys, and the sorted key list all agree with the oracle.
#[test]
fn oracle_agrees_on_hash_map_lookup() {
    let atoms = AtomTable::new();
    let n = 40usize;
    let (_buf, map) = build_int_map(n, &atoms);
    // SAFETY: `map` is a live boxed map with `n > MAP_SMALL_MAP_LIMIT` keys.
    assert!(unsafe { is_hashmap(map) }, "40 keys must build a hash map");
    assert_eq!(map_size(map, &atoms).unwrap(), n);

    // Every present key, plus absent ones (0, n+1, 1000).
    let lookup: Vec<i64> = (1..=n as i64).chain([0, n as i64 + 1, 1000]).collect();

    // The Rust side.
    let bt_vals: Vec<Option<i64>> = lookup
        .iter()
        .map(|&k| {
            map_get(map, make_small(k), &atoms)
                .unwrap()
                .map(signed_value)
        })
        .collect();
    let bt_keys: Vec<Option<i64>> = map_pairs(map, &atoms)
        .unwrap()
        .iter()
        .map(|(k, _)| Some(signed_value(*k)))
        .collect();

    let lookup_list: Vec<String> = lookup.iter().map(|k| k.to_string()).collect();
    let expr = format!(
        "M = {}, Keys = [{}], \
         io:format(\"~w|~w|~w\", [maps:size(M), lists:sort(maps:keys(M)), \
         [case maps:find(K,M) of {{ok,V}} -> V; error -> none end || K <- Keys]])",
        erl_int_map_literal(n),
        lookup_list.join(","),
    );
    let Some(out) = erl_stdout(&expr) else {
        return;
    };
    let fields = split_fields(&out);
    assert_eq!(fields.len(), 3, "unexpected oracle output: {out:?}");

    assert_eq!(parse_int(fields[0]), n as i64, "oracle maps:size");
    assert_eq!(parse_int_list(fields[1]), bt_keys, "oracle maps:keys");
    assert_eq!(parse_int_list(fields[2]), bt_vals, "oracle maps:get results");
}

/// An **atom-keyed** map: atom immediates used as keys interoperate with the oracle
/// (`maps:get` by atom name, including a missing key).
#[test]
fn oracle_agrees_on_atom_keyed_map() {
    let mut atoms = AtomTable::new();
    atoms.seed(["a", "b", "c", "d", "e"]);
    let present = ["a", "b", "c", "d"];
    let pairs: Vec<(Eterm, Eterm)> = present
        .iter()
        .enumerate()
        .map(|(i, name)| {
            (
                make_atom(atoms.lookup(name).unwrap()),
                make_small(i as i64 + 1),
            )
        })
        .collect();
    let (_buf, map) = build_map(&pairs, &atoms);
    assert_eq!(map_size(map, &atoms).unwrap(), present.len());

    // `e` is interned but absent from the map.
    let lookup = ["a", "b", "c", "d", "e"];
    let bt_vals: Vec<Option<i64>> = lookup
        .iter()
        .map(|name| {
            let key = make_atom(atoms.lookup(name).unwrap());
            map_get(map, key, &atoms).unwrap().map(signed_value)
        })
        .collect();

    let expr = "M = #{a=>1,b=>2,c=>3,d=>4}, Keys = [a,b,c,d,e], \
                io:format(\"~w|~w\", [maps:size(M), \
                [case maps:find(K,M) of {ok,V} -> V; error -> none end || K <- Keys]])";
    let Some(out) = erl_stdout(expr) else {
        return;
    };
    let fields = split_fields(&out);
    assert_eq!(fields.len(), 2, "unexpected oracle output: {out:?}");

    assert_eq!(parse_int(fields[0]), present.len() as i64, "oracle maps:size");
    assert_eq!(parse_int_list(fields[1]), bt_vals, "oracle maps:get results");
}

/// Non-byte-aligned and byte-aligned bitstrings: `bit_size`, the individual bits
/// (MSB-first) and the leading full bytes agree with the oracle.
#[test]
fn oracle_agrees_on_bitstring_bits_and_bytes() {
    let cases: Vec<(Vec<u8>, usize)> = vec![
        (vec![0x20], 3),          // <<1:3>>
        (vec![0b1011_0000], 4),   // <<11:4>>
        (vec![0x80], 1),          // <<1:1>>
        (vec![0xFF], 8),          // <<255:8>>
        (vec![0xAB, 0xC0], 10),   // <<0x2AF:10>> == 1010101111
        (b"hello".to_vec(), 40),  // <<"hello">>
        (vec![0, 1, 2, 255], 32), // <<0,1,2,255>>
        (Vec::new(), 0),          // <<>>
    ];

    for (bytes, n_bits) in &cases {
        let n_bits = *n_bits;
        let (_buf, t) = build_heap_bits(bytes, n_bits);

        // SAFETY: `t` is a live on-heap bitstring.
        let (base, offset, size) = unsafe { bitstring_parts(t) };
        assert_eq!(offset, 0, "on-heap bitstring offset, {bytes:?}/{n_bits}");
        assert_eq!(size, n_bits, "beam_term bit size, {bytes:?}/{n_bits}");

        let bt_bits: Vec<Option<i64>> = (0..n_bits)
            .map(|i| Some(unsafe { bit_at(t, i) } as i64))
            .collect();

        // The payload is `ceil(n_bits / 8)` bytes; the oracle can only report the
        // whole leading bytes, so compare the first `n_bits / 8` of them.
        let n_bytes = bit_count_to_bytes(n_bits);
        let bt_full_bytes: Vec<Option<i64>> = unsafe { std::slice::from_raw_parts(base, n_bytes) }
            .iter()
            .take(n_bits / 8)
            .map(|b| Some(*b as i64))
            .collect();

        let byte_list: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
        let expr = format!(
            "Bytes = [{}], Bits = {}, Bin = << <<X:8>> || X <- Bytes >>, \
             <<P:Bits/bits,_/bits>> = Bin, \
             io:format(\"~w|~w|~w\", [bit_size(P), [X || <<X:1>> <= P], [Y || <<Y:8>> <= P]])",
            byte_list.join(","),
            n_bits,
        );
        let Some(out) = erl_stdout(&expr) else {
            return;
        };
        let fields = split_fields(&out);
        assert_eq!(fields.len(), 3, "unexpected oracle output: {out:?}");

        assert_eq!(
            parse_int(fields[0]),
            n_bits as i64,
            "oracle bit_size, {bytes:?}/{n_bits}",
        );
        assert_eq!(
            parse_int_list(fields[1]),
            bt_bits,
            "oracle bits, {bytes:?}/{n_bits}",
        );
        assert_eq!(
            parse_int_list(fields[2]),
            bt_full_bytes,
            "oracle full bytes, {bytes:?}/{n_bits}",
        );
    }
}

/// A byte-aligned **binary** built with `write_binary`: `bit_size` and the exact
/// byte contents agree with the oracle.
#[test]
fn oracle_agrees_on_binary_contents() {
    let payload: Vec<u8> = b"beam_term".to_vec();
    let n_bits = payload.len() * 8;

    let words = heap_bits_size_words(n_bits);
    let mut buf = vec![SENTINEL; words + 8];
    // SAFETY: `buf` owns `words + 8 >= heap_bits_size_words(payload.len() * 8)` words.
    let t = unsafe { write_binary(buf.as_mut_ptr(), &payload) }.expect("write_binary");
    // SAFETY: `t` is a live on-heap binary.
    let (base, offset, size) = unsafe { bitstring_parts(t) };
    assert_eq!((offset, size), (0, n_bits), "binary offset/size");
    // SAFETY: the payload covers `payload.len()` bytes from `base`.
    let got = unsafe { std::slice::from_raw_parts(base, payload.len()) };
    assert_eq!(got, &payload[..], "beam_term binary contents");

    let byte_list: Vec<String> = payload.iter().map(|b| b.to_string()).collect();
    let expr = format!(
        "Bytes = [{}], B = << <<X:8>> || X <- Bytes >>, \
         io:format(\"~w|~w\", [bit_size(B), [Y || <<Y:8>> <= B]])",
        byte_list.join(","),
    );
    let Some(out) = erl_stdout(&expr) else {
        return;
    };
    let fields = split_fields(&out);
    assert_eq!(fields.len(), 2, "unexpected oracle output: {out:?}");

    assert_eq!(parse_int(fields[0]), n_bits as i64, "oracle bit_size");
    let expected: Vec<Option<i64>> = payload.iter().map(|b| Some(*b as i64)).collect();
    assert_eq!(parse_int_list(fields[1]), expected, "oracle binary contents");
}

// ---------------------------------------------------------------------------
// Bignum
// ---------------------------------------------------------------------------

/// The bignum cases exercised against the oracle: `(Erlang literal, negative,
/// limbs)`, limbs least-significant-first (`ErtsDigit`s, one `u64` each).
///
/// These are exactly the values that expose the header-arity convention: a
/// bignum with `n` limbs is `big_size_words(n) == n + 1` words and its header
/// arity **is** `n` (`big.h:70`), so `1 bsl 134` (3 limbs) must not compare equal
/// to `63 bsl 128` (3 limbs, different MSB limb) even though the writer of an
/// off-by-one layout would collapse them.
const BIG_CASES: &[(&str, bool, &[u64])] = &[
    ("(1 bsl 64)", false, &[0, 1]),          // 2 limbs, flat_size 3
    ("(1 bsl 70)", false, &[0, 64]),         // 2 limbs, flat_size 3
    ("(1 bsl 134)", false, &[0, 0, 64]),     // 3 limbs, flat_size 4
    ("(63 bsl 128)", false, &[0, 0, 63]),    // 3 limbs, flat_size 4
    ("-(1 bsl 100)", true, &[0, 68719476736]), // 2 limbs, flat_size 3
    ("(1 bsl 128)", false, &[0, 0, 1]),      // 3 limbs, flat_size 4
];

/// `size_object` of every bignum case agrees with the oracle's
/// `erts_debug:flat_size/1` (one header word plus one word per digit).
#[test]
fn oracle_agrees_on_bignum_flat_size() {
    for (erl, negative, digits) in BIG_CASES {
        let (_buf, t) = build_big(*negative, digits);

        let expr = format!("io:format(\"~w\", [erts_debug:flat_size({erl})])");
        let Some(out) = erl_stdout(&expr) else {
            return;
        };
        assert_eq!(
            size_object(t) as i64,
            parse_int(&out),
            "size_object vs erts_debug:flat_size({erl})",
        );
        assert_eq!(
            size_object(t),
            big_size_words(digits.len()),
            "size_object({erl}) must be big_size_words({})",
            digits.len(),
        );
    }
}

/// `eq` and `cmp` agree with the oracle's `=:=` / `<` / `>` for the pairs that
/// distinguish the digit-count convention: a differing MSB limb on the same limb
/// count, an equal value, and a sign difference.
#[test]
fn oracle_agrees_on_bignum_equality_and_order() {
    let atoms = AtomTable::new();
    let pairs = [
        ("(1 bsl 134)", "(63 bsl 128)"), // differ only in the top limb -> not equal, A > B
        ("(63 bsl 128)", "(1 bsl 134)"),
        ("(1 bsl 70)", "(1 bsl 70)"),    // equal
        ("(1 bsl 64)", "(1 bsl 128)"),   // 2 limbs vs 3 limbs -> A < B
        ("-(1 bsl 100)", "(1 bsl 64)"),  // negative vs positive -> A < B
        ("(1 bsl 128)", "(1 bsl 64)"),   // 3 limbs vs 2 limbs -> A > B
    ];

    // Map each literal back to its limbs so the Rust term is built from the same
    // input as the oracle expression.
    let build = |erl: &str| -> (Vec<Eterm>, Eterm) {
        let (_, negative, digits) = BIG_CASES
            .iter()
            .find(|(e, _, _)| *e == erl)
            .unwrap_or_else(|| panic!("no big case for {erl}"));
        build_big(*negative, digits)
    };

    for (erl_a, erl_b) in pairs {
        let (_ba, a) = build(erl_a);
        let (_bb, b) = build(erl_b);

        let expr = format!(
            "io:format(\"~w|~w|~w\", \
             [{erl_a} =:= {erl_b}, {erl_a} < {erl_b}, {erl_a} > {erl_b}])"
        );
        let Some(out) = erl_stdout(&expr) else {
            return;
        };
        let fields = split_fields(&out);
        assert_eq!(fields.len(), 3, "unexpected oracle output: {out:?}");
        let (o_eq, o_lt, o_gt) = (
            parse_bool(fields[0]),
            parse_bool(fields[1]),
            parse_bool(fields[2]),
        );

        assert_eq!(eq(a, b, &atoms), o_eq, "eq({erl_a}, {erl_b})");
        let expected = if o_lt {
            Ordering::Less
        } else if o_gt {
            Ordering::Greater
        } else {
            Ordering::Equal
        };
        assert_eq!(cmp(a, b, &atoms), expected, "cmp({erl_a}, {erl_b})");
        assert_eq!(
            cmp(b, a, &atoms),
            expected.reverse(),
            "cmp({erl_b}, {erl_a}) must reverse",
        );
    }
}

/// The arithmetic comparison of a bignum beyond `2^53` against a float uses the
/// approximate `f64` value (`number_as_f64`); `(1 bsl 70) > 1.0` is `true`, which
/// requires the top limb to be read.
#[test]
fn oracle_agrees_on_bignum_vs_float_arithmetic() {
    let atoms = AtomTable::new();
    let (_bb, big) = build_big(false, &[0, 64]); // 1 bsl 70
    let (_bf, one) = build_float(1.0);

    let Some(out) = erl_stdout("io:format(\"~w\", [(1 bsl 70) > 1.0])") else {
        return;
    };
    let oracle = parse_bool(&out);
    assert!(oracle, "sanity: the oracle reports (1 bsl 70) > 1.0");

    assert_eq!(
        cmp_arith(big, one, &atoms),
        Ordering::Greater,
        "cmp_arith(1 bsl 70, 1.0)",
    );
    assert_eq!(
        cmp_arith(one, big, &atoms),
        Ordering::Less,
        "cmp_arith(1.0, 1 bsl 70)",
    );
}

/// `term_hash` consistency (`eq(a, b) ⇒ term_hash(a) == term_hash(b)`) plus a
/// regression guard: two bignums differing only in the **top** limb must hash
/// differently (a hash that dropped the top limb would collide them).
#[test]
fn bignum_hash_sees_the_top_limb() {
    let atoms = AtomTable::new();
    let (_b1, same_a) = build_big(false, &[0, 0, 64]); // 1 bsl 134
    let (_b2, same_b) = build_big(false, &[0, 0, 64]); // same value
    let (_b3, diff) = build_big(false, &[0, 0, 63]); // 63 bsl 128, top limb differs

    assert!(eq(same_a, same_b, &atoms), "same-value bignums are =:=");
    assert_eq!(
        term_hash(same_a, &atoms),
        term_hash(same_b, &atoms),
        "eq(a, b) must imply equal hashes",
    );

    assert!(!eq(same_a, diff, &atoms), "1 bsl 134 =/= 63 bsl 128");
    assert_ne!(
        term_hash(same_a, &atoms),
        term_hash(diff, &atoms),
        "bignums differing only in the top limb must hash differently",
    );
}

// ---------------------------------------------------------------------------
// `make_hash2` / `make_hash` cross-check (the `erlang:phash2` / `erlang:phash`
// compatibility contract)
// ---------------------------------------------------------------------------
//
// Unlike `term_hash` (whose values are internal), `make_hash2` and `make_hash`
// **are** the observable `erlang:phash2/1,2` / `erlang:phash/2` results, so they
// are a Level-1 drop-in contract: for every term `T`,
//
//   `make_hash2(T) & ((1 << 27) - 1) == erlang:phash2(T)`
//   `make_hash2(T)                   == erlang:phash2(T, 1 bsl 32)`
//   `make_hash(T)  % R               == erlang:phash(T, R) - 1`   (1 <= R < 2^32)
//   `make_hash(T)                    == erlang:phash(T, 1 bsl 32) - 1`
//
// (the `2^32` range is special-cased in `bif.c:5413-5452` to return the raw
// hash). Every term below is described **once** by a [`Spec`] and materialized
// twice — as a `beam_term` term and as Erlang source text — so the comparison is
// a genuine oracle agreement rather than a tautology.

/// A term described once and materialized both as a `beam_term` term (the Rust
/// side) and as Erlang source (the oracle side).
#[derive(Clone, Debug)]
enum Spec {
    Small(i64),
    /// An atom, named by its UTF-8 bytes (safe for non-ASCII / non-latin1 names).
    Atom(&'static str),
    /// The empty list `[]` (the `NIL` immediate).
    Nil,
    Tuple(Vec<Spec>),
    /// A proper list.
    List(Vec<Spec>),
    /// An improper list `[head.. | tail]`.
    Improper(Vec<Spec>, Box<Spec>),
    /// A bitstring of `n_bits` bits. `bytes` holds `ceil(n_bits / 8)` MSB-first
    /// bytes with the unused low bits of the last byte already zero (the
    /// `ErlHeapBits` convention).
    Bits(Vec<u8>, usize),
    Float(f64),
    /// A multi-limb bignum (`digits` least-significant-first, canonical: the top
    /// limb is non-zero and the value does not fit a small integer).
    Big { negative: bool, digits: Vec<u64> },
    Map(Vec<(Spec, Spec)>),
    /// An `EXPORT_EXT` external fun `fun Module:Function/Arity`.
    ExternalFun {
        module: &'static str,
        function: &'static str,
        arity: usize,
    },
}

/// A bump arena over a fixed, sentinel-guarded word buffer. Every term built into
/// it stays valid for the arena's lifetime because the backing `Vec` never
/// reallocates (nothing ever pushes to it).
struct Bump {
    buf: Vec<Eterm>,
    used: usize,
}

impl Bump {
    fn new() -> Self {
        Bump {
            buf: vec![SENTINEL; 1 << 16],
            used: 0,
        }
    }

    /// Reserves `n` words and returns a pointer to the first (still `SENTINEL`).
    fn alloc(&mut self, n: usize) -> *mut Eterm {
        assert!(self.used + n <= self.buf.len(), "term arena exhausted");
        // SAFETY: `used + n <= buf.len()`, so the range is in bounds, and `buf`
        // is never pushed to, so its base pointer is stable for the arena's life.
        let p = unsafe { self.buf.as_mut_ptr().add(self.used) };
        self.used += n;
        p
    }
}

/// Materializes `spec` into the arena as a real `beam_term` term.
fn build(spec: &Spec, bump: &mut Bump, atoms: &mut AtomTable) -> Eterm {
    match spec {
        Spec::Small(v) => make_small(*v),
        Spec::Atom(name) => make_atom(atoms.intern(name)),
        Spec::Nil => NIL,
        Spec::Tuple(elems) => {
            if elems.is_empty() {
                // The shared, `TAG_LITERAL_PTR`-tagged empty tuple.
                return tuple0();
            }
            let vals: Vec<Eterm> = elems.iter().map(|e| build(e, bump, atoms)).collect();
            let p = bump.alloc(vals.len() + 1);
            // SAFETY: `p` owns `vals.len() + 1` writable words — an arityval block
            // is `[arityval(n), elements..]`.
            unsafe {
                *p = make_arityval(vals.len());
                for (i, v) in vals.iter().enumerate() {
                    *p.add(1 + i) = *v;
                }
            }
            make_boxed(p)
        }
        Spec::List(head) => build_list(head, NIL, bump, atoms),
        Spec::Improper(head, tail) => {
            let tail = build(tail, bump, atoms);
            build_list(head, tail, bump, atoms)
        }
        Spec::Bits(bytes, n_bits) => {
            let p = bump.alloc(heap_bits_size_words(*n_bits));
            // SAFETY: `p` owns exactly `heap_bits_size_words(n_bits)` words.
            unsafe { write_heap_bits(p, bytes, *n_bits) }.expect("write_heap_bits")
        }
        Spec::Float(v) => {
            let p = bump.alloc(FLOAT_SIZE_WORDS);
            // SAFETY: `p` owns `FLOAT_SIZE_WORDS` words.
            unsafe { write_float(p, *v) }
        }
        Spec::Big { negative, digits } => {
            let p = bump.alloc(big_size_words(digits.len()));
            // SAFETY: `p` owns `big_size_words(digits.len())` words.
            unsafe { write_big(p, *negative, digits) }
        }
        Spec::Map(pairs) => {
            let built: Vec<(Eterm, Eterm)> = pairs
                .iter()
                .map(|(k, v)| (build(k, bump, atoms), build(v, bump, atoms)))
                .collect();
            let p = bump.alloc(map_words_capacity(built.len()));
            // SAFETY: `p` owns `map_words_capacity(built.len())` words.
            unsafe { write_map(p, &built, atoms) }.expect("write_map")
        }
        Spec::ExternalFun {
            module,
            function,
            arity,
        } => {
            let m = atoms.intern(module);
            let f = atoms.intern(function);
            let p = bump.alloc(fun_words(FunKind::External, 0));
            // SAFETY: `p` owns `fun_words(FunKind::External, 0)` (3) words.
            unsafe { write_fun_external(p, *arity, m, f) }.expect("write_fun_external")
        }
    }
}

/// Builds the cons-cell chain `head..` ending in `tail`.
fn build_list(head: &[Spec], tail: Eterm, bump: &mut Bump, atoms: &mut AtomTable) -> Eterm {
    let mut out = tail;
    for e in head.iter().rev() {
        let car = build(e, bump, atoms);
        let p = bump.alloc(2);
        // SAFETY: `p` owns two writable words — a header-less `[CAR, CDR]` cell.
        unsafe {
            *p = car;
            *p.add(1) = out;
        }
        out = make_list(p);
    }
    out
}

/// The Erlang source for `spec` — the same description `build` materializes.
fn erl_literal(spec: &Spec) -> String {
    match spec {
        Spec::Small(v) => v.to_string(),
        Spec::Atom(name) => erl_atom(name),
        Spec::Nil => "[]".to_string(),
        Spec::Tuple(elems) => format!(
            "{{{}}}",
            elems.iter().map(erl_literal).collect::<Vec<_>>().join(",")
        ),
        Spec::List(elems) => format!(
            "[{}]",
            elems.iter().map(erl_literal).collect::<Vec<_>>().join(",")
        ),
        Spec::Improper(head, tail) => format!(
            "[{}|{}]",
            head.iter().map(erl_literal).collect::<Vec<_>>().join(","),
            erl_literal(tail)
        ),
        Spec::Bits(bytes, n_bits) => erl_bits(bytes, *n_bits),
        Spec::Float(v) => erl_float(*v),
        Spec::Big { negative, digits } => erl_big(*negative, digits),
        Spec::Map(pairs) => format!(
            "#{{{}}}",
            pairs
                .iter()
                .map(|(k, v)| format!("({}) => ({})", erl_literal(k), erl_literal(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Spec::ExternalFun {
            module,
            function,
            arity,
        } => format!("fun {module}:{function}/{arity}"),
    }
}

/// The Erlang source for an atom of the given UTF-8 name, emitted as
/// `binary_to_atom(<<bytes>>, utf8)` so quoting, escaping and non-ASCII names need
/// no special cases.
fn erl_atom(name: &str) -> String {
    let bytes: Vec<String> = name.as_bytes().iter().map(u8::to_string).collect();
    format!("binary_to_atom(<<{}>>, utf8)", bytes.join(","))
}

/// The Erlang source for a bitstring of `n_bits` bits from MSB-first `bytes`. The
/// trailing `Rem:lastbits` segment takes the **high** bits of the last byte, which
/// is exactly the `ErlHeapBits` MSB-first convention.
fn erl_bits(bytes: &[u8], n_bits: usize) -> String {
    let full = n_bits / 8;
    let rem = n_bits % 8;
    let mut parts: Vec<String> = bytes[..full].iter().map(u8::to_string).collect();
    if rem > 0 {
        parts.push(format!("{}:{}", bytes[full] >> (8 - rem), rem));
    }
    format!("<<{}>>", parts.join(","))
}

/// The Erlang source for an `f64`, built from its IEEE-754 bits — a decimal
/// literal would risk a rounding mismatch. The anonymous fun gives the literal its
/// own variable scope, so several floats in one expression cannot clash.
fn erl_float(v: f64) -> String {
    let bits = v.to_bits();
    let bytes: Vec<String> = (0..8)
        .map(|i| ((bits >> (56 - 8 * i)) & 0xff).to_string())
        .collect();
    format!("fun() -> <<V:64/float>> = <<{}>>, V end()", bytes.join(","))
}

/// The Erlang source for a bignum of least-significant-first `u64` limbs.
fn erl_big(negative: bool, digits: &[u64]) -> String {
    let sum = digits
        .iter()
        .enumerate()
        .map(|(i, d)| {
            if i == 0 {
                d.to_string()
            } else {
                format!("({d} bsl {})", i * 64)
            }
        })
        .collect::<Vec<_>>()
        .join(" + ");
    if negative {
        format!("(-({sum}))")
    } else {
        format!("({sum})")
    }
}

/// The term corpus: every class `make_hash2` / `make_hash` support, with the
/// boundary values that select their different code paths (the 28-bit small fast
/// path, the byte-list batching, the block-hash 12-byte boundary, the
/// flat-map/hash-map threshold, the 2-/3-limb bignum boundary).
fn hash_specs() -> Vec<Spec> {
    let mut s: Vec<Spec> = Vec::new();

    // Small integers: the 28-bit fast-path boundary, powers of two, and the 60-bit
    // small-integer limits.
    for v in [
        0i64,
        1,
        -1,
        2,
        127,
        128,
        255,
        256,
        -256,
        (1 << 27) - 1,
        1 << 27,
        -(1 << 27),
        -(1 << 27) - 1,
        (1 << 28) - 1,
        1 << 28,
        -(1 << 28),
        1 << 32,
        -(1 << 32),
        1 << 40,
        -(1 << 40),
        MAX_SMALL,
        MIN_SMALL,
        MAX_SMALL - 1,
        MIN_SMALL + 1,
    ] {
        s.push(Spec::Small(v));
    }

    // Atoms: the empty atom, ASCII names, and non-ASCII / non-latin1 UTF-8 names
    // (these exercise `AtomTable::hvalue`'s latin1 clutch and its raw-UTF-8 path).
    for a in [
        "", "a", "abc", "nil", "true", "false", "lists", "sort", "¶", "Ω", "Ωmega", "😀",
    ] {
        s.push(Spec::Atom(a));
    }

    // The empty list.
    s.push(Spec::Nil);

    // Tuples: arity 0..4, a nested/heterogeneous shape and a longer one.
    s.push(Spec::Tuple(vec![]));
    s.push(Spec::Tuple(vec![Spec::Small(1)]));
    s.push(Spec::Tuple(vec![Spec::Small(1), Spec::Small(2)]));
    s.push(Spec::Tuple(vec![
        Spec::Small(1),
        Spec::Small(2),
        Spec::Small(3),
    ]));
    s.push(Spec::Tuple(vec![
        Spec::Atom("a"),
        Spec::Tuple(vec![Spec::Small(1), Spec::Small(2)]),
        Spec::List(vec![Spec::Small(1), Spec::Small(2)]),
        Spec::Bits(vec![0x20], 3),
    ]));
    s.push(Spec::Tuple((0..12).map(Spec::Small).collect()));

    // Lists: byte batches of 1..4 elements per `UINT32_HASH` word, a non-byte
    // element, atom elements, nested lists, and improper tails.
    for n in [1usize, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13] {
        s.push(Spec::List(
            (0..n as i64).map(|i| Spec::Small(255 - (i * 7) % 200)).collect(),
        ));
    }
    s.push(Spec::List(vec![
        Spec::Small(97),
        Spec::Small(98),
        Spec::Small(99),
    ]));
    s.push(Spec::List(vec![
        Spec::Small(256),
        Spec::Small(1),
        Spec::Small(2),
    ]));
    s.push(Spec::List(vec![
        Spec::Atom("a"),
        Spec::Atom("b"),
        Spec::Atom("c"),
    ]));
    s.push(Spec::List(vec![
        Spec::List(vec![Spec::Small(1)]),
        Spec::List(vec![Spec::Small(2), Spec::Small(3)]),
    ]));
    s.push(Spec::Improper(
        vec![Spec::Small(1), Spec::Small(2)],
        Box::new(Spec::Small(3)),
    ));
    s.push(Spec::Improper(vec![Spec::Small(1)], Box::new(Spec::Nil)));
    s.push(Spec::Improper(
        vec![Spec::Small(1), Spec::Small(2)],
        Box::new(Spec::Atom("tail")),
    ));

    // Maps: empty, flat (`<= 32` pairs), atom-keyed, and hash maps (33 / 40 keys,
    // straddling `MAP_SMALL_MAP_LIMIT`).
    s.push(Spec::Map(vec![]));
    s.push(Spec::Map(vec![(Spec::Small(1), Spec::Small(2))]));
    s.push(Spec::Map(vec![
        (Spec::Small(1), Spec::Atom("one")),
        (Spec::Small(2), Spec::Atom("two")),
        (
            Spec::Small(3),
            Spec::List(vec![Spec::Small(1), Spec::Small(2)]),
        ),
    ]));
    s.push(Spec::Map(vec![
        (Spec::Atom("a"), Spec::Small(1)),
        (Spec::Atom("b"), Spec::Small(2)),
    ]));
    for n in [33usize, 40] {
        s.push(Spec::Map(
            (1..=n as i64)
                .map(|i| (Spec::Small(i), Spec::Small(i * 10)))
                .collect(),
        ));
    }

    // Bitstrings: empty, non-byte-aligned, the block-hash 12-byte boundary, and a
    // 100-bit string (multi-byte-block + trailing partial byte).
    s.push(Spec::Bits(vec![], 0));
    s.push(Spec::Bits(vec![0x20], 3)); // <<1:3>>
    s.push(Spec::Bits(vec![0xF0], 4)); // <<15:4>>
    s.push(Spec::Bits(vec![0xAB, 0xC0], 10)); // a 10-bit window
    for n in [1usize, 4, 11, 12, 13, 24, 100] {
        let bytes: Vec<u8> = (0..n).map(|i| (i % 251) as u8 + 1).collect();
        s.push(Spec::Bits(bytes, n * 8));
    }
    s.push(Spec::Bits((0..13).map(|i| (i * 3) as u8).collect(), 100));

    // Floats, including both zeros (`make_hash2` normalizes `-0.0` to `+0.0`).
    for v in [
        0.0f64,
        -0.0,
        1.0,
        -1.0,
        0.5,
        1.5,
        -2.5,
        0.1,
        3.141_592_653_589_793,
        1.0e300,
        -1.0e300,
        1.0e-300,
        4.9e-324,
        f64::MAX,
        f64::MIN,
        -12345.6789,
    ] {
        s.push(Spec::Float(v));
    }

    // Bignums: multi-limb, positive and negative, across the 2-/3-limb boundary.
    s.push(Spec::Big {
        negative: false,
        digits: vec![1 << 59], // 2^59, just above MAX_SMALL
    });
    s.push(Spec::Big {
        negative: false,
        digits: vec![0, 1], // 2^64
    });
    s.push(Spec::Big {
        negative: false,
        digits: vec![1, 1], // 2^64 + 1
    });
    s.push(Spec::Big {
        negative: false,
        digits: vec![0, 0, 1], // 2^128
    });
    s.push(Spec::Big {
        negative: false,
        digits: vec![u64::MAX, u64::MAX], // 2^128 - 1
    });
    s.push(Spec::Big {
        negative: true,
        digits: vec![1 << 59],
    });
    s.push(Spec::Big {
        negative: true,
        digits: vec![0, 1],
    });
    s.push(Spec::Big {
        negative: true,
        digits: vec![0, 0, 1],
    });
    s.push(Spec::Big {
        negative: true,
        digits: vec![0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321],
    });

    // External (`EXPORT_EXT`) funs — the only fun kind both sides can produce
    // (a `make_fun3` local fun is not constructible from `-eval`, and an
    // `ErlFunEntry`-backed import needs an MD5/serialize pair the oracle will not
    // hand out; see the crate `CONTEXT.md`).
    for (module, function, arity) in [
        ("lists", "sort", 1usize),
        ("lists", "reverse", 1),
        ("erlang", "length", 1),
    ] {
        s.push(Spec::ExternalFun {
            module,
            function,
            arity,
        });
    }

    s
}

/// Parses an Erlang list of integers printed by `~w`, e.g. `[1,2,3]`.
fn parse_ints(s: &str) -> Vec<i64> {
    parse_int_list(s)
        .into_iter()
        .map(|v| v.expect("no `none` sentinel expected here"))
        .collect()
}

/// The `erlang:phash2/1,2` / `erlang:phash/2` compatibility contract, pinned
/// against the live oracle over a broad term corpus ([`hash_specs`]).
#[test]
fn oracle_agrees_on_make_hash2_and_make_hash() {
    let specs = hash_specs();
    let mut bump = Bump::new();
    let mut atoms = AtomTable::new();
    let terms: Vec<Eterm> = specs
        .iter()
        .map(|s| build(s, &mut bump, &mut atoms))
        .collect();

    let literals: Vec<String> = specs.iter().map(erl_literal).collect();
    let expr = format!(
        "Ts = [{}], io:format(\"~w|~w|~w|~w|~w|~w|~w|~w\", \
         [[erlang:phash2(T) || T <- Ts], \
          [erlang:phash2(T, 1 bsl 32) || T <- Ts], \
          [erlang:phash(T, 1) - 1 || T <- Ts], \
          [erlang:phash(T, 2) - 1 || T <- Ts], \
          [erlang:phash(T, 3) - 1 || T <- Ts], \
          [erlang:phash(T, 1000) - 1 || T <- Ts], \
          [erlang:phash(T, 4294967295) - 1 || T <- Ts], \
          [erlang:phash(T, 1 bsl 32) - 1 || T <- Ts]])",
        literals.join(","),
    );
    let Some(out) = erl_stdout(&expr) else {
        return;
    };
    let fields = split_fields(&out);
    assert_eq!(fields.len(), 8, "unexpected oracle output: {out:?}");

    let phash2_1 = parse_ints(fields[0]);
    let phash2_2p32 = parse_ints(fields[1]);
    let phash_r1 = parse_ints(fields[2]);
    let phash_r2 = parse_ints(fields[3]);
    let phash_r3 = parse_ints(fields[4]);
    let phash_r1000 = parse_ints(fields[5]);
    let phash_rmax = parse_ints(fields[6]);
    let phash_2p32 = parse_ints(fields[7]);
    for (name, v) in [
        ("phash2/1", &phash2_1),
        ("phash2/2", &phash2_2p32),
        ("phash/2 r=1", &phash_r1),
        ("phash/2 r=2", &phash_r2),
        ("phash/2 r=3", &phash_r3),
        ("phash/2 r=1000", &phash_r1000),
        ("phash/2 r=2^32-1", &phash_rmax),
        ("phash/2 r=2^32", &phash_2p32),
    ] {
        assert_eq!(v.len(), specs.len(), "oracle {name} list length");
    }

    for (i, spec) in specs.iter().enumerate() {
        let t = terms[i];

        let h2 = make_hash2(t, &atoms)
            .unwrap_or_else(|e| panic!("make_hash2({spec:?}) failed: {e}"))
            as i64;
        assert_eq!(
            h2, phash2_2p32[i],
            "make_hash2({spec:?}) must equal erlang:phash2(T, 1 bsl 32)"
        );
        assert_eq!(
            h2 & ((1 << 27) - 1),
            phash2_1[i],
            "make_hash2({spec:?}) & (2^27-1) must equal erlang:phash2(T)"
        );

        // A bare atom additionally pins `AtomTable::hvalue` directly: the
        // `hash == 0` fast path returns `atom_tab(a)->slot.bucket.hvalue` as the
        // whole hash.
        if let Spec::Atom(name) = spec {
            let atom = atoms.lookup(name).expect("interned above");
            assert_eq!(
                h2 as u32,
                atoms.hvalue(atom),
                "make_hash2(atom {name:?}) must be AtomTable::hvalue"
            );
        }

        let h = make_hash(t, &atoms).unwrap_or_else(|e| panic!("make_hash({spec:?}) failed: {e}"))
            as i64;
        assert_eq!(
            h, phash_2p32[i],
            "make_hash({spec:?}) must equal erlang:phash(T, 1 bsl 32) - 1"
        );
        assert_eq!(h % 1, phash_r1[i], "make_hash({spec:?}) % 1");
        assert_eq!(h % 2, phash_r2[i], "make_hash({spec:?}) % 2");
        assert_eq!(h % 3, phash_r3[i], "make_hash({spec:?}) % 3");
        assert_eq!(h % 1000, phash_r1000[i], "make_hash({spec:?}) % 1000");
        assert_eq!(
            h % 4_294_967_295,
            phash_rmax[i],
            "make_hash({spec:?}) % (2^32-1)"
        );
    }
}

// ---------------------------------------------------------------------------
// Flat-map key order (`cmp_flatmap_keys` / the stored key order)
// ---------------------------------------------------------------------------
//
// A flat map stores its keys in `erts_cmp_flatmap_keys` order
// (`erts/emulator/beam/erl_utils.h:267-272`), which is reproduced by
// [`cmp_flatmap_keys`]: two **atom** keys order by atom-table *index* (the C's
// `key_a - key_b`), every other pair by the full term order [`cmp`]. The two
// checks below pin that against the live oracle through its observable
// `maps:to_list/1` order (a small map is flat, so its list order *is* the stored
// key order), for the two interesting cases: binary keys (byte-lexicographic) and
// atom keys (deliberately *not* name-ordered).

/// Binary keys order byte-lexicographically: the stored key order is
/// `<<"bin">> < <<"float">> < <<"list">>`, regardless of the insertion order (the
/// pairs are deliberately inserted list-first, so an insertion-ordered map would
/// disagree), and the oracle's `maps:to_list/1` reports the same ordinals.
#[test]
fn oracle_agrees_on_flatmap_binary_key_order() {
    let atoms = AtomTable::new();

    // The three keys as on-heap binaries; each returned buffer owns the payload its
    // key term points into and must stay alive while the map is read.
    let key_bytes: [&[u8]; 3] = [b"bin", b"float", b"list"];
    let mut bufs: Vec<Vec<Eterm>> = Vec::new();
    let keys: Vec<Eterm> = key_bytes
        .iter()
        .map(|bytes| {
            let (buf, t) = build_heap_bits(bytes, bytes.len() * 8);
            bufs.push(buf);
            t
        })
        .collect();
    let (bin_key, float_key, list_key) = (keys[0], keys[1], keys[2]);

    // Deliberately non-sorted insertion order (list, bin, float) with the ordinals
    // (2, 0, 1); the stored order must still be bin(0) < float(1) < list(2).
    let pairs = [
        (list_key, make_small(2)),
        (bin_key, make_small(0)),
        (float_key, make_small(1)),
    ];
    let (_buf, map) = build_map(&pairs, &atoms);
    // SAFETY: `map` is a live boxed map with three pairs.
    assert!(unsafe { is_flatmap(map) }, "3 keys must build a flat map");

    let stored: Vec<i64> = map_pairs_storage_order(map)
        .expect("map_pairs_storage_order")
        .iter()
        .map(|(_, v)| signed_value(*v))
        .collect();
    assert_eq!(
        stored,
        vec![0, 1, 2],
        "flat-map stored order must be bin < float < list",
    );
    assert_ne!(
        stored,
        vec![2, 0, 1],
        "flat-map stored order must not be the insertion order",
    );

    assert_eq!(
        cmp_flatmap_keys(bin_key, float_key, &atoms),
        Ordering::Less,
        "cmp_flatmap_keys(<<\"bin\">>, <<\"float\">>)",
    );
    assert_eq!(
        cmp_flatmap_keys(list_key, bin_key, &atoms),
        Ordering::Greater,
        "cmp_flatmap_keys(<<\"list\">>, <<\"bin\">>)",
    );

    let expr = "M = #{<<\"bin\">>=>0,<<\"float\">>=>1,<<\"list\">>=>2}, \
                io:format(\"~w\", [[I || {_,I} <- maps:to_list(M)]])";
    let Some(out) = erl_stdout(expr) else {
        eprintln!("skipping flat-map binary-key order check: no oracle");
        return;
    };
    let oracle: Vec<Option<i64>> = parse_int_list(&out);
    let stored_as_options: Vec<Option<i64>> = stored.iter().map(|v| Some(*v)).collect();
    assert_eq!(
        oracle, stored_as_options,
        "oracle maps:to_list ordinals vs the stored key order",
    );
}

/// Atom keys order by atom-table **index**, not by name: `"zzzq_two"` is interned
/// before `"zzzq_one"` (so it has the lower index) while sorting *after* it
/// lexicographically, so the stored key order is `two, one` (= ordinals `[2, 1]`)
/// and the oracle's `maps:to_list/1` reports the same (deliberately
/// non-name-sorted) order.
#[test]
fn oracle_agrees_on_flatmap_atom_key_order() {
    let mut atoms = AtomTable::new();
    // Intern in an order opposite to the name order: `zzzq_two` gets the lower index.
    atoms.seed(["zzzq_two", "zzzq_one"]);
    let two = make_atom(atoms.lookup("zzzq_two").expect("interned"));
    let one = make_atom(atoms.lookup("zzzq_one").expect("interned"));
    assert!(
        atom_val(two).raw() < atom_val(one).raw(),
        "test premise: `zzzq_two` is interned before `zzzq_one`",
    );

    let pairs = [(two, make_small(2)), (one, make_small(1))];
    let (_buf, map) = build_map(&pairs, &atoms);
    // SAFETY: `map` is a live boxed map with two pairs.
    assert!(unsafe { is_flatmap(map) }, "2 keys must build a flat map");

    let stored: Vec<i64> = map_pairs_storage_order(map)
        .expect("map_pairs_storage_order")
        .iter()
        .map(|(_, v)| signed_value(*v))
        .collect();
    assert_eq!(
        stored,
        vec![2, 1],
        "atom keys order by index, so `zzzq_two`(2) precedes `zzzq_one`(1)",
    );

    assert_eq!(
        cmp_flatmap_keys(two, one, &atoms),
        Ordering::Less,
        "cmp_flatmap_keys(zzzq_two, zzzq_one) must order by index",
    );
    assert_eq!(
        cmp_flatmap_keys(one, two, &atoms),
        Ordering::Greater,
        "cmp_flatmap_keys(zzzq_one, zzzq_two) must order by index",
    );

    let expr = "A2 = list_to_atom(\"zzzq_two\"), A1 = list_to_atom(\"zzzq_one\"), \
                M = #{A2=>2,A1=>1}, \
                io:format(\"~w\", [[I || {_,I} <- maps:to_list(M)]])";
    let Some(out) = erl_stdout(expr) else {
        eprintln!("skipping flat-map atom-key order check: no oracle");
        return;
    };
    let oracle_ordinals: Vec<i64> = parse_int_list(&out)
        .into_iter()
        .map(|v| v.expect("no `none` in a maps:to_list ordinal list"))
        .collect();
    assert_eq!(oracle_ordinals.len(), 2, "unexpected oracle output: {out:?}");

    // Derive the expectation from the oracle rather than guessing it.
    assert_eq!(
        stored, oracle_ordinals,
        "stored atom-key order must equal the oracle's maps:to_list order",
    );
    // Teeth: the oracle's order is the NON-name-sorted one (`zzzq_one` would sort
    // before `zzzq_two` lexicographically), so *index* order drives the stored order.
    assert_ne!(
        oracle_ordinals,
        vec![1, 2],
        "the order must not be the lexicographic name order (that would prove nothing)",
    );
}