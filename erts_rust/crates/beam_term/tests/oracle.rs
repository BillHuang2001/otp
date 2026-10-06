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
