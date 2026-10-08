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

//! Live-oracle cross-check of `beam_term`'s bignum → `f64` conversion
//! ([`beam_term::number::big_to_f64`] / [`beam_term::number::number_as_f64`])
//! against the **real system Erlang** (`erl`, OTP 29 / erts-17.1).
//!
//! # What is being checked
//!
//! `big_to_f64` ports ERTS's `big_to_double` (`erts/emulator/beam/big.c:2412-2529`),
//! whose caller `i_fconv` (`erts/emulator/beam/emu/float_instrs.tab:38-52`) raises
//! `badarith` for a value that is not a finite double. `erlang:float/1`
//! (`erts/emulator/beam/erl_bif_guard.c:125`) calls the *same* `big_to_double` and
//! reports an out-of-range value with `badarg` instead — the conversion result, which
//! is what this test pins, is identical. This crate-local slice of the workspace
//! *compatibility gate* asks the oracle to convert a large corpus of integers and
//! compares the resulting IEEE-754 bits with `big_to_f64`.
//!
//! The corpus is:
//! * a **systematic bit-length sweep** `1..=1100` of three shapes per width (a lone
//!   top bit, all-ones, and a pseudo-random pattern), both signs — this walks the
//!   mantissa ties-to-even behaviour, the 1024-bit finite/overflow boundary and
//!   many-limb (≥3 limbs) values;
//! * a batch of **randomised** multi-limb values of random widths and signs;
//! * a small set of **curated** vectors around the rounding boundaries.
//!
//! # Protocol (same convention as `tests/oracle.rs`)
//!
//! This crate cannot dev-depend on `beam_codec` (it would close a dependency cycle),
//! so instead of re-implementing the External Term Format the check talks to `erl`
//! over a **text protocol**: the oracle is spawned with `-noshell -eval` and prints
//! each result as the raw IEEE-754 bits (`<<F:64/float>>` → `binary_to_list`, i.e.
//! the 8 big-endian bytes) or the atom `overflow` when `float/1` refuses. The Rust
//! side builds its bignum from the *same* limbs — and the Erlang integer literal from
//! the same limbs via an arbitrary-precision decimal conversion — so agreement is a
//! genuine oracle agreement, not a tautology.
//!
//! # Absence of the oracle is not a failure
//!
//! If `erl` is not on `PATH` (or cannot be spawned / does not answer within
//! [`ERL_TIMEOUT`]) every check **skips loudly** (a clear `eprintln!`) and returns,
//! never failing the suite, so `cargo test -p beam_term` stays green without Erlang.

use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_term::prelude::*;

/// Wall-clock budget for a single `erl` invocation before the oracle is declared
/// unusable and the affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(60);

/// Number of integers per `erl` invocation. Bounded so the `-eval` argument stays
/// well under the OS single-argument limit (a 1100-bit value is ~330 decimal chars).
const BATCH: usize = 100;

/// A word written around every bignum buffer to detect overruns of the reserved
/// capacity: `write_big` must never touch it.
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
            eprintln!("skipping number oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping number oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
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

// ---------------------------------------------------------------------------
// Integer ↔ text and term builders (the Rust side of every comparison)
// ---------------------------------------------------------------------------

/// The value `sum(limbs[i] << (64 * i))` as a decimal string (arbitrary-precision,
/// schoolbook base-10^9). This is how the **same** limbs are rendered as an Erlang
/// integer literal.
fn limbs_to_decimal(limbs: &[u64]) -> String {
    const BASE: u128 = 1_000_000_000;
    let mut dec: Vec<u32> = vec![0]; // little-endian base-1e9
    for &limb in limbs.iter().rev() {
        // dec = dec * 2^64 + limb
        let mut carry = limb as u128;
        for d in dec.iter_mut() {
            let t = ((*d as u128) << 64) + carry;
            *d = (t % BASE) as u32;
            carry = t / BASE;
        }
        while carry > 0 {
            dec.push((carry % BASE) as u32);
            carry /= BASE;
        }
    }
    let mut s = String::new();
    let mut it = dec.iter().rev();
    s.push_str(&it.next().unwrap().to_string());
    for d in it {
        s.push_str(&format!("{d:09}"));
    }
    s
}

/// The Erlang integer literal (decimal, optionally signed) for `limbs`.
fn erl_int_literal(negative: bool, limbs: &[u64]) -> String {
    let mut s = limbs_to_decimal(limbs);
    if negative {
        s.insert(0, '-');
    }
    s
}

/// Builds a bignum from `limbs` (least-significant-first `u64`) into a
/// sentinel-guarded buffer and returns the buffer plus the tagged root.
fn build_big(negative: bool, limbs: &[u64]) -> (Vec<Eterm>, Eterm) {
    assert!(!limbs.is_empty(), "a bignum needs at least one limb");
    assert_ne!(*limbs.last().unwrap(), 0, "a bignum must be normalised");
    let words = big_size_words(limbs.len());
    let mut buf = vec![SENTINEL; words + 8];
    // SAFETY: `buf` owns `words + 8 >= big_size_words(limbs.len())` words;
    // `write_big` writes exactly `big_size_words(limbs.len())` of them.
    let t = unsafe { write_big(buf.as_mut_ptr(), negative, limbs) };
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

// ---------------------------------------------------------------------------
// The batched oracle query
// ---------------------------------------------------------------------------

/// Asks the oracle to convert every spec's integer to `f64`, returning one entry per
/// spec: `Some(bits-as-f64)` on success, `None` when `float/1` raised (out of range).
/// Returns `None` (after a loud skip) when the oracle is unavailable.
fn oracle_batch(specs: &[(bool, Vec<u64>)]) -> Option<Vec<Option<f64>>> {
    assert!(!specs.is_empty());
    let ints: Vec<String> = specs
        .iter()
        .map(|(neg, limbs)| erl_int_literal(*neg, limbs))
        .collect();
    let expr = format!(
        "Enc = fun(X) -> try binary_to_list(<<(float(X)):64/float>>) \
           catch _:_ -> overflow end end, \
         L = [{ints}], \
         Out = [case Enc(X) of overflow -> \"overflow\"; \
                B -> lists:join(\",\", [integer_to_list(Y) || Y <- B]) end || X <- L], \
         io:format(\"~s\", [lists:join(\";\", Out)])",
        ints = ints.join(","),
    );
    let out = erl_stdout(&expr)?;
    Some(parse_oracle(&out, specs.len()))
}

/// Parses the oracle's `bits;bits;...` output (`overflow` for a refused value).
fn parse_oracle(out: &str, n: usize) -> Vec<Option<f64>> {
    let chunks: Vec<&str> = out.trim().split(';').collect();
    assert_eq!(
        chunks.len(),
        n,
        "expected {n} oracle results, got {}: {out:?}",
        chunks.len(),
    );
    chunks
        .iter()
        .map(|c| {
            let c = c.trim();
            if c == "overflow" {
                None
            } else {
                let bytes: Vec<u8> = c
                    .split(',')
                    .map(|b| {
                        b.trim()
                            .parse::<u8>()
                            .unwrap_or_else(|e| panic!("bad oracle byte {b:?} in {out:?}: {e}"))
                    })
                    .collect();
                let arr: [u8; 8] = bytes
                    .try_into()
                    .unwrap_or_else(|v: Vec<u8>| panic!("expected 8 bytes, got {}", v.len()));
                Some(f64::from_be_bytes(arr))
            }
        })
        .collect()
}

/// Compares `big_to_f64` / `number_as_f64` with the oracle for every spec, batching
/// the oracle calls. Skips loudly (and returns) if the oracle is unavailable.
fn compare_with_oracle(specs: &[(bool, Vec<u64>)]) {
    for batch in specs.chunks(BATCH) {
        let Some(oracle) = oracle_batch(batch) else {
            return;
        };
        assert_eq!(oracle.len(), batch.len());
        for ((neg, limbs), want) in batch.iter().zip(oracle) {
            let (_buf, t) = build_big(*neg, limbs);
            match (big_to_f64(t), want) {
                (Some(got), Some(w)) => {
                    assert_eq!(
                        got.to_bits(),
                        w.to_bits(),
                        "big_to_f64 = {got:e} but the oracle = {w:e} for {neg} {limbs:?}",
                    );
                    assert_eq!(
                        number_as_f64(t).to_bits(),
                        w.to_bits(),
                        "number_as_f64 disagrees with the oracle for {neg} {limbs:?}",
                    );
                }
                (None, None) => {
                    let inf = if *neg {
                        f64::NEG_INFINITY
                    } else {
                        f64::INFINITY
                    };
                    assert_eq!(
                        number_as_f64(t),
                        inf,
                        "unrepresentable bignum must give ±INFINITY for {neg} {limbs:?}",
                    );
                }
                (got, want) => panic!(
                    "big_to_f64 = {got:?} but the oracle = {want:?} for {neg} {limbs:?}",
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Corpora
// ---------------------------------------------------------------------------

/// Limbs of `2^(bitlen-1)` (a lone top bit), `bitlen >= 1`.
fn lone_top_bit(bitlen: usize) -> Vec<u64> {
    let n = bitlen.div_ceil(64);
    let mut v = vec![0u64; n];
    v[(bitlen - 1) / 64] = 1u64 << ((bitlen - 1) % 64);
    v
}

/// Limbs of `2^bitlen - 1` (all `bitlen` low bits set).
fn all_ones(bitlen: usize) -> Vec<u64> {
    let n = bitlen.div_ceil(64);
    let mut v = vec![u64::MAX; n];
    let rem = bitlen % 64;
    if rem != 0 {
        *v.last_mut().unwrap() = (1u64 << rem) - 1;
    }
    v
}

/// The systematic sweep: every width `1..=1100` in three shapes, both signs.
fn sweep_specs() -> Vec<(bool, Vec<u64>)> {
    let mut st = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::new();
    for bitlen in 1..=1100usize {
        let rand = {
            let mut v = vec![0u64; bitlen.div_ceil(64)];
            for w in v.iter_mut() {
                *w = xorshift(&mut st);
            }
            let rem = bitlen % 64;
            if rem != 0 {
                *v.last_mut().unwrap() &= (1u64 << rem) - 1;
            }
            v[(bitlen - 1) / 64] |= 1u64 << ((bitlen - 1) % 64);
            v
        };
        for limbs in [lone_top_bit(bitlen), all_ones(bitlen), rand] {
            out.push((false, limbs.clone()));
            out.push((true, limbs));
        }
    }
    out
}

/// Randomised multi-limb values: a random width, random limbs, a random sign.
fn random_specs(count: usize) -> Vec<(bool, Vec<u64>)> {
    let mut st = 0x0BAD_C0FF_EE0D_DF00u64;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        // Skew the width across the whole representable range, favouring the top.
        let bitlen = 1 + (xorshift(&mut st) as usize % 1152);
        let n = bitlen.div_ceil(64);
        let mut limbs: Vec<u64> = (0..n).map(|_| xorshift(&mut st)).collect();
        let rem = bitlen % 64;
        if rem != 0 {
            *limbs.last_mut().unwrap() &= (1u64 << rem) - 1;
        }
        // Force normalisation (top bit set) unless that zeroes the top limb.
        limbs[(bitlen - 1) / 64] |= 1u64 << ((bitlen - 1) % 64);
        let negative = xorshift(&mut st) & 1 == 1;
        out.push((negative, limbs));
    }
    out
}

/// xorshift64 — a tiny deterministic PRNG (no external deps).
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The full bit-length sweep against the live oracle.
#[test]
fn oracle_agrees_on_bit_length_sweep() {
    compare_with_oracle(&sweep_specs());
}

/// Randomised many-limb values against the live oracle.
#[test]
fn oracle_agrees_on_random_bignums() {
    compare_with_oracle(&random_specs(1200));
}

/// A curated set of boundary vectors: the oracle result is pinned to the exact bits
/// this crate also expects, so the check is meaningful even if the pins ever drift.
#[test]
fn oracle_agrees_on_curated_boundary_vectors() {
    let two_pow = |e: i32| 2.0f64.powi(e);
    let max = {
        let mut v = vec![0u64; 16];
        v[15] = u64::MAX << 11; // 2^1024 - 2^971
        v
    };
    let halfway = {
        let mut v = vec![0u64; 16];
        v[15] = u64::MAX << 10; // 2^1024 - 2^970
        v
    };
    let bits = |b: &[usize]| -> Vec<u64> {
        let maxb = *b.iter().max().unwrap();
        let mut v = vec![0u64; maxb / 64 + 1];
        for &i in b {
            v[i / 64] |= 1u64 << (i % 64);
        }
        v
    };
    let hundred = {
        let mut v = bits(&[100, 47]);
        v[0] += 12345;
        v
    };

    // (name, negative, limbs, expected f64)
    let cases: Vec<(&str, bool, Vec<u64>, Option<f64>)> = vec![
        ("2^53 + 1", false, vec![(1u64 << 53) + 1], Some(two_pow(53))),
        ("2^54 + 2", false, vec![(1u64 << 54) + 2], Some(two_pow(54))),
        ("2^54 + 6", false, vec![(1u64 << 54) + 6], Some(two_pow(54) + 8.0)),
        ("2^70 - 1", false, vec![u64::MAX, (1u64 << 6) - 1], Some(two_pow(70))),
        (
            "2^117 + 2^64 + 1",
            false,
            bits(&[117, 64, 0]),
            Some(f64::from_bits(0x4740_0000_0000_0001)),
        ),
        ("2^70 + 2^17", false, bits(&[70, 17]), Some(two_pow(70))),
        (
            "2^70 + 2^17 + 2^16",
            false,
            bits(&[70, 17, 16]),
            Some(f64::from_bits(0x4450_0000_0000_0001)),
        ),
        (
            "2^70 + 2^18 + 2^17",
            false,
            bits(&[70, 18, 17]),
            Some(f64::from_bits(0x4450_0000_0000_0002)),
        ),
        // Multi-limb tie-break descent (the C walks down through all-zero limbs).
        ("2^200 + 2^147", false, bits(&[200, 147]), Some(two_pow(200))),
        (
            "2^200 + 2^147 + 1",
            false,
            bits(&[200, 147, 0]),
            Some(f64::from_bits(0x4C70_0000_0000_0001)),
        ),
        ("2^300 + 2^247", false, bits(&[300, 247]), Some(two_pow(300))),
        (
            "2^300 + 2^247 + 2^100",
            false,
            bits(&[300, 247, 100]),
            Some(f64::from_bits(0x52B0_0000_0000_0001)),
        ),
        (
            "2^200 + 2^148 + 2^147",
            false,
            bits(&[200, 148, 147]),
            Some(f64::from_bits(0x4C70_0000_0000_0002)),
        ),
        ("2^1023 + 2^970", false, bits(&[1023, 970]), Some(two_pow(1023))),
        (
            "2^1023 + 2^970 + 1",
            false,
            bits(&[1023, 970, 0]),
            Some(f64::from_bits(0x7FE0_0000_0000_0001)),
        ),
        (
            "2^100 + 2^47 + 12345",
            false,
            hundred,
            Some(f64::from_bits(0x4630_0000_0000_0001)),
        ),
        ("2^200 + 1", false, bits(&[200, 0]), Some(two_pow(200))),
        ("2^1023", false, bits(&[1023]), Some(f64::from_bits(0x7FE0_0000_0000_0000))),
        ("f64::MAX", false, max, Some(f64::MAX)),
        ("-(2^70 - 1)", true, vec![u64::MAX, (1u64 << 6) - 1], Some(-two_pow(70))),
        ("2^1024 - 2^970", false, halfway, None),
        ("2^1024 - 1", false, vec![u64::MAX; 16], None),
    ];

    // Query the oracle for just these values and confirm it hashes out to our pins.
    let specs: Vec<(bool, Vec<u64>)> = cases
        .iter()
        .map(|(_, neg, limbs, _)| (*neg, limbs.clone()))
        .collect();
    let Some(oracle) = oracle_batch(&specs) else {
        return;
    };
    for ((name, neg, limbs, want), saw) in cases.iter().zip(oracle) {
        assert_eq!(saw, *want, "the oracle result for {name} drifted from the pinned value");
        let (_buf, t) = build_big(*neg, limbs);
        match (big_to_f64(t), want) {
            (Some(got), Some(w)) => assert_eq!(got.to_bits(), w.to_bits(), "big_to_f64({name})"),
            (None, None) => assert_eq!(
                number_as_f64(t),
                if *neg { f64::NEG_INFINITY } else { f64::INFINITY },
                "number_as_f64({name})",
            ),
            (got, w) => panic!("big_to_f64({name}) = {got:?} but expected {w:?}"),
        }
    }
}
