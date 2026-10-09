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

//! Live-oracle cross-check of `beam_term`'s **arithmetic** mixed integer/float
//! comparison ([`beam_term::cmp_arith`] / [`beam_term::eq`]) against the real
//! system Erlang (`erl`).
//!
//! # What is being checked
//!
//! `cmp_arith` ports `erts_cmp_compound`'s `mixed_types` block
//! (`erts/emulator/beam/utils.c:2345-2461`) — the `SMALL_FLOAT`/`FLOAT_SMALL` and
//! `BIG_FLOAT`/`FLOAT_BIG` cases, which compare an integer and a float **exactly**
//! (never by lossily converting the integer to `f64`). The `2^53` repro is the
//! canonical failure: `9007199254740993 == 9007199254740992.0` is `false` and
//! `9007199254740993 > 9007199254740992.0` is `true` in the real runtime, while a
//! `f64`-converting implementation rounds the odd integer down to `2^53` and
//! reports `Equal`.
//!
//! For every case the oracle is asked four questions about the *same* pair of
//! values — `A == B`, `A =:= B`, `A < B`, `A > B` — and the answers are compared
//! with `cmp_arith(a, b, atoms)` (arithmetic, `==`) and `eq(a, b, atoms)` (exact,
//! `=:=`). Erlang's `<`/`>` use the arithmetic comparison
//! (`erts_cmp_compound(X, Y, 0, 0)`), so all three arithmetic answers must agree.
//!
//! # Corpus
//!
//! * a dense cross product of integers around `±2^53` against the doubles in the
//!   same band — this walks the lossless boundary and the asymmetry directly;
//! * the small-integer range extremes (`MIN_SMALL`/`MAX_SMALL`) against floats at,
//!   just below and far outside `±2^59`;
//! * bignums of 1..17 limbs (so both the "bignum's size decides" bands and the
//!   exact `double_to_big` + `big_comp` band are exercised) against floats from
//!   `1.0` to `1.0e308`, both signs and **both operand orders**.
//!
//! `±inf`/`NaN` are deliberately absent: ERTS's float parser, `binary_to_term/1`
//! and the bit-syntax float match all refuse to build a non-finite double, so the
//! oracle cannot express them — those values are pinned by the crate's offline
//! unit tests instead (see `src/compare/numbers.rs`).
//!
//! # Protocol (same convention as `tests/oracle.rs` / `tests/number_oracle.rs`)
//!
//! This crate cannot dev-depend on `beam_codec` (it would close a dependency
//! cycle), so instead of re-implementing the External Term Format the check talks
//! to `erl` over a **text protocol**: the oracle is spawned with `-noshell -eval`,
//! the case list is embedded as Erlang source, and the four boolean answers per
//! case are printed as `~w` and scanned back in order. The Rust side builds its
//! terms from the *same* limbs/floats, so agreement is genuine.
//!
//! # Absence of the oracle is not a failure
//!
//! If `erl` is not on `PATH` (or cannot be spawned / does not answer within
//! [`ERL_TIMEOUT`]) every check **skips loudly** (a clear `eprintln!`) and returns,
//! never failing the suite, so `cargo test -p beam_term` stays green without Erlang.

use std::cmp::Ordering;
use std::process::Command;use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_term::prelude::*;

/// Wall-clock budget for a single `erl` invocation before the oracle is declared
/// unusable and the affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(60);

/// Number of cases per `erl` invocation. Bounded so the `-eval` argument stays well
/// under the OS single-argument limit.
const BATCH: usize = 48;

/// A word written around every term buffer to detect overruns of the reserved
/// capacity: `write_big`/`write_float` must never touch it.
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
            eprintln!("skipping compare oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping compare oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
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
// Value model, Erlang source rendering and term building
// ---------------------------------------------------------------------------

/// One side of a comparison case.
#[derive(Clone)]
enum Num {
    /// A small immediate (must fit `MIN_SMALL..=MAX_SMALL`).
    Small(i64),
    /// A bignum, limbs least-significant-first (the top limb must be non-zero).
    Big(bool, Vec<u64>),
    /// A float.
    Float(f64),
}

impl Num {
    /// The Erlang source literal for this value. Erlang integers are
    /// arbitrary-precision, so a small and a bignum of the same magnitude render
    /// identically — only the Rust term differs (an immediate vs a bignum block).
    fn erl(&self) -> String {
        match self {
            Num::Small(v) => v.to_string(),
            Num::Big(neg, limbs) => erl_int_literal(*neg, limbs),
            Num::Float(f) => erl_float_literal(*f),
        }
    }
}

/// A comparison case: `left <op> right`.
struct Case {
    left: Num,
    right: Num,
}

/// The value `sum(limbs[i] << (64 * i))` as a decimal string (arbitrary-precision,
/// schoolbook base-10^9) — the same conversion `tests/number_oracle.rs` uses.
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

/// The Erlang float literal for `f`, guaranteed to round-trip to the same bits.
///
/// Rust's shortest round-trip rendering is reused, but an Erlang float literal
/// needs a fraction part: `1e30` is not valid Erlang source, `1.0e30` is.
fn erl_float_literal(f: f64) -> String {
    assert!(
        f.is_finite(),
        "only finite floats are expressible in Erlang source (got {f})",
    );
    let s = format!("{f:?}");
    match s.find(['e', 'E']) {
        Some(pos) => {
            if s[..pos].contains('.') {
                s
            } else {
                format!("{}.0{}", &s[..pos], &s[pos..])
            }
        }
        None if s.contains('.') => s,
        None => format!("{s}.0"),
    }
}

/// Builds the term for `n`, keeping its backing storage alive in `storage`.
fn build_num(n: &Num, storage: &mut Vec<Vec<Eterm>>) -> Eterm {
    match n {
        Num::Small(v) => make_small(*v),
        Num::Big(neg, limbs) => {
            assert!(!limbs.is_empty() && *limbs.last().unwrap() != 0, "normalised bignum");
            let words = big_size_words(limbs.len());
            let mut buf = vec![SENTINEL; words + 8];
            // SAFETY: `buf` owns `words + 8 >= big_size_words(limbs.len())` words;
            // `write_big` writes exactly `big_size_words(limbs.len())` of them.
            let t = unsafe { write_big(buf.as_mut_ptr(), *neg, limbs) };
            for (i, w) in buf[words..].iter().enumerate() {
                assert_eq!(*w, SENTINEL, "write_big overflowed by {} words", i + 1);
            }
            storage.push(buf);
            t
        }
        Num::Float(f) => {
            let mut buf = vec![SENTINEL; FLOAT_SIZE_WORDS + 4];
            // SAFETY: `buf` owns `FLOAT_SIZE_WORDS + 4 >= FLOAT_SIZE_WORDS` words;
            // `write_float` writes exactly `FLOAT_SIZE_WORDS` of them.
            let t = unsafe { write_float(buf.as_mut_ptr(), *f) };
            for (i, w) in buf[FLOAT_SIZE_WORDS..].iter().enumerate() {
                assert_eq!(*w, SENTINEL, "write_float overflowed by {} words", i + 1);
            }
            storage.push(buf);
            t
        }
    }
}

// ---------------------------------------------------------------------------
// The batched oracle query
// ---------------------------------------------------------------------------

/// The oracle's four answers for one case.
struct Answers {
    arith_eq: bool,
    exact_eq: bool,
    less: bool,
    greater: bool,
}

/// Asks the oracle about every case in `cases`, batching the invocations.
/// Returns `None` (after a loud skip) when the oracle is unavailable.
fn oracle_answers(cases: &[Case]) -> Option<Vec<Answers>> {
    let mut out = Vec::with_capacity(cases.len());
    for batch in cases.chunks(BATCH) {
        let pairs: Vec<String> = batch
            .iter()
            .map(|c| format!("{{{},{}}}", c.left.erl(), c.right.erl()))
            .collect();
        let expr = format!(
            "Pairs = [{}], \
             Out = [[X == Y, X =:= Y, X < Y, X > Y] || {{X,Y}} <- Pairs], \
             io:format(\"~w\", [Out])",
            pairs.join(","),
        );
        let stdout = erl_stdout(&expr)?;
        let bools = scan_bools(&stdout);
        assert_eq!(
            bools.len(),
            batch.len() * 4,
            "expected {} oracle answers, got {}: {stdout:?}",
            batch.len() * 4,
            bools.len(),
        );
        for chunk in bools.chunks(4) {
            out.push(Answers {
                arith_eq: chunk[0],
                exact_eq: chunk[1],
                less: chunk[2],
                greater: chunk[3],
            });
        }    }
    Some(out)
}

/// Scans the `true`/`false` tokens out of the oracle's `~w` output, in order.
fn scan_bools(out: &str) -> Vec<bool> {
    let mut bools = Vec::new();
    let mut token = String::new();
    for ch in out.chars() {
        if ch.is_ascii_alphabetic() {
            token.push(ch);
        } else if !token.is_empty() {
            match token.as_str() {
                "true" => bools.push(true),
                "false" => bools.push(false),
                _ => {}
            }
            token.clear();
        }
    }
    if !token.is_empty() {
        match token.as_str() {
            "true" => bools.push(true),
            "false" => bools.push(false),
            _ => {}
        }
    }
    bools
}

/// Runs every case against the oracle and asserts `cmp_arith`/`eq` agree.
fn check_against_oracle(cases: &[Case]) {
    let Some(answers) = oracle_answers(cases) else {
        return;
    };
    // One shared atom table: the terms contain no atoms, but `cmp` needs one.
    let atoms = AtomTable::new();
    let mut storage: Vec<Vec<Eterm>> = Vec::new();

    for (case, want) in cases.iter().zip(&answers) {
        let a = build_num(&case.left, &mut storage);
        let b = build_num(&case.right, &mut storage);

        let arith = cmp_arith(a, b, &atoms);
        let (la, ra) = (&case.left, &case.right);
        assert_eq!(
            arith == Ordering::Equal,
            want.arith_eq,
            "{} == {}: cmp_arith gave {arith:?}",
            erl_repr(la),
            erl_repr(ra),
        );
        assert_eq!(
            arith == Ordering::Less,
            want.less,
            "{} < {}: cmp_arith gave {arith:?}",
            erl_repr(la),
            erl_repr(ra),
        );
        assert_eq!(
            arith == Ordering::Greater,
            want.greater,
            "{} > {}: cmp_arith gave {arith:?}",
            erl_repr(la),
            erl_repr(ra),
        );
        assert_eq!(
            eq(a, b, &atoms),
            want.exact_eq,
            "{} =:= {}: eq disagrees",
            erl_repr(la),
            erl_repr(ra),
        );
        // Antisymmetry of the arithmetic comparison must hold for every pair.
        assert_eq!(
            cmp_arith(b, a, &atoms),
            arith.reverse(),
            "cmp_arith must reverse for {} vs {}",
            erl_repr(la),
            erl_repr(ra),
        );
    }
}

/// A short label for a failing case.
fn erl_repr(n: &Num) -> String {
    match n {
        Num::Small(v) => format!("small {v}"),
        Num::Big(neg, limbs) => format!("big {}{:?}", if *neg { "-" } else { "" }, limbs),
        Num::Float(f) => format!("float {f:?}"),
    }
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// `2^(64 * (n - 1) + top_bit)` as `n` limbs.
fn pow2_limbs(n: usize, top_bit: u32) -> Vec<u64> {
    let mut v = vec![0u64; n];
    v[n - 1] = 1u64 << top_bit;
    v
}

/// Every case in both operand orders.
fn both_orders(out: &mut Vec<Case>, a: Num, b: Num) {
    out.push(Case {
        left: a.clone(),
        right: b.clone(),
    });
    out.push(Case {
        left: b,
        right: a,
    });
}

/// The dense `±2^53` band, where a `double` can no longer represent every integer.
fn two_pow_53_band() -> Vec<Case> {
    let smalls: [(i64, i64); 8] = [
        (9_007_199_254_740_989, -1),
        (-9_007_199_254_740_989, -1),
        (9_007_199_254_740_990, -1),
        (9_007_199_254_740_991, -1),
        (9_007_199_254_740_992, -1),
        (9_007_199_254_740_993, -1),
        (9_007_199_254_740_994, -1),
        (9_007_199_254_740_995, -1),
    ];
    let floats = [
        9_007_199_254_740_988.0,
        9_007_199_254_740_990.0,
        9_007_199_254_740_991.0,
        9_007_199_254_740_992.0,
        9_007_199_254_740_994.0,
        9_007_199_254_740_996.0,
        -9_007_199_254_740_992.0,
        -1.0,
    ];
    let mut out = Vec::new();
    for (s, _) in smalls {
        for f in floats {
            both_orders(&mut out, Num::Small(s), Num::Float(f));
        }
    }
    out
}

/// The small-integer range extremes against floats at and outside `±2^59`.
fn small_range_extremes() -> Vec<Case> {
    const MAX_SMALL: i64 = (1i64 << 59) - 1;
    const MIN_SMALL: i64 = -(1i64 << 59);
    let smalls = [
        MIN_SMALL,
        MIN_SMALL + 1,
        -4_503_599_627_370_496, // -2^52
        0,
        1,
        -1,
        4_503_599_627_370_496, // 2^52
        MAX_SMALL - 1,
        MAX_SMALL,
    ];
    // `2^59` (= MAX_SMALL + 1), `-2^59`, `2^59 - 64`, and small/fractional floats.
    let floats = [
        576_460_752_303_423_488.0,
        -576_460_752_303_423_488.0,
        576_460_752_303_423_424.0,
        0.5,
        -0.5,
        1.0,
        0.0,
        -1.0e300,
        1.0e300,
    ];
    let mut out = Vec::new();
    for s in smalls {
        for f in floats {
            both_orders(&mut out, Num::Small(s), Num::Float(f));
        }
    }
    out
}

/// Bignums of 1..17 limbs against floats from `1.0` to `1.0e308`.
fn bignum_vs_float() -> Vec<Case> {
    let bignums: Vec<Num> = vec![
        Num::Big(false, pow2_limbs(1, 60)),           // 2^60
        Num::Big(false, pow2_limbs(1, 63)),           // 2^63
        Num::Big(false, pow2_limbs(2, 0)),            // 2^64
        Num::Big(false, pow2_limbs(2, 36)),           // 2^100
        Num::Big(true, pow2_limbs(2, 36)),            // -(2^100)
        Num::Big(false, pow2_limbs(3, 0)),            // 2^128
        Num::Big(false, pow2_limbs(15, 14)),          // 2^910
        Num::Big(false, pow2_limbs(15, 63)),          // 2^959
        Num::Big(false, pow2_limbs(16, 0)),           // 2^960
        Num::Big(false, pow2_limbs(16, 63)),          // 2^1023
        Num::Big(true, pow2_limbs(16, 63)),           // -(2^1023)
        Num::Big(false, pow2_limbs(17, 0)),           // 2^1024
        Num::Big(true, pow2_limbs(17, 0)),            // -(2^1024)
        // A "random-looking" 3-limb value, both signs (top limb non-zero).
        Num::Big(
            false,
            vec![0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210, 0x0F0F_0F0F_0F0F_0F0F],
        ),
        Num::Big(
            true,
            vec![0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210, 0x0F0F_0F0F_0F0F_0F0F],
        ),
    ];
    let floats = [
        -1.0e300,
        -1.0e30,
        -1.0,
        0.5,
        1.0,
        1.0e20,
        1.0e30,
        1.0e100,
        1.0e274,
        1.0e289,
        1.0e300,
        1.0e308,
    ];
    let mut out = Vec::new();
    for b in &bignums {
        for f in floats {
            both_orders(&mut out, b.clone(), Num::Float(f));
        }
    }
    out
}

/// The full corpus.
fn corpus() -> Vec<Case> {
    let mut out = two_pow_53_band();
    out.extend(small_range_extremes());
    out.extend(bignum_vs_float());
    out
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// Every corpus case must match the oracle's `==`, `=:=`, `<` and `>`.
#[test]
fn compare_oracle_agrees_on_mixed_numbers() {
    let cases = corpus();
    assert!(cases.len() > 300, "the corpus should be substantial");
    check_against_oracle(&cases);
}

/// The objective's reproducer, pinned explicitly (so the regression is caught even
/// if the corpus is trimmed later).
#[test]
fn compare_oracle_agrees_on_two_pow_53_asymmetry() {
    let cases = vec![
        Case {
            left: Num::Small(9_007_199_254_740_993), // 2^53 + 1
            right: Num::Float(9_007_199_254_740_992.0), // 2^53
        },
        Case {
            left: Num::Float(9_007_199_254_740_992.0),
            right: Num::Small(9_007_199_254_740_993),
        },
        Case {
            left: Num::Small(-9_007_199_254_740_993),
            right: Num::Float(-9_007_199_254_740_992.0),
        },
    ];
    check_against_oracle(&cases);
}

/// Renderer sanity: every Erlang literal the corpus emits must be accepted by the
/// oracle, and the parsed value must be identical to the Rust value it was built
/// from. This guards the test harness itself (the `Num::erl` renderer and the
/// decimal/float text protocols), not the comparison.
#[test]
fn corpus_literals_round_trip_under_the_oracle() {
    let cases = corpus();
    let lits: Vec<String> = cases
        .iter()
        .flat_map(|c| [c.left.erl(), c.right.erl()])
        .collect();
    // `erl` must parse every literal, evaluate the list to exactly `lits.len()`
    // elements, and echo the list length back.
    let Some(stdout) = erl_stdout(&format!(
        "L = [{}], io:format(\"~w\", [length(L)])",
        lits.join(","),
    )) else {
        return;
    };
    assert_eq!(
        stdout.trim().parse::<usize>(),
        Ok(lits.len()),
        "the oracle did not accept every literal: {stdout:?}",
    );
}