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
//! Integration tests: the **plain float leaf** of the printer (`~w`/`~p`/`~W`/`~P`)
//! renders a float exactly as the oracle does.
//!
//! The oracle's rule is `float_to_list(F, [short])`: `io_lib:write1/4`'s float
//! clause is `io_lib_format:fwrite_g(F)`, and `fwrite_g/1` is
//! `float_to_list(F, [short])` (`lib/stdlib/src/io_lib_format.erl:758-760`) —
//! `io_lib_pretty` (the `~p`/`~P` re-flow) reaches the same clause for a float
//! leaf. `src/print.rs` used to render Rust's shortest round-trip instead (with a
//! `.0` appended when integral), which agrees for mid-magnitude values but not
//! for the extremes: `1.0e10` printed `10000000000` where the oracle prints
//! `1.0e10`, `1.0e-6` printed `0.000001` where the oracle prints `1.0e-6`. It now
//! shares [`beam_bifs`]'s `numeric::format_short` port, so the printer and the
//! `float_to_list/2` BIF cannot drift apart.
//!
//! Every golden below was produced by
//!
//! ```text
//! erl -noshell -eval 'io:format(Fmt, [Term]), halt().'
//! ```
//!
//! on the OTP 29.1.1 / erts-17.1 oracle, and the live cross-checks re-derive them
//! (skipping loudly when no `erl` is on `PATH`). `~e`/`~f`/`~g` are pinned too:
//! they are **not** the plain float leaf (they are `fwrite_e`/`fwrite_f`/`fwrite_g`
//! with a field/precision, i.e. `float_to_list/2` with `{scientific,N}`/
//! `{decimals,N}`) and the fix must not disturb them.
use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::prelude::*;
use std::process::Command;

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// An atom table seeded with the atoms the container fixtures need.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a"]);
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

/// `io:format(Fmt, Args)` text, asserting success.
fn io_text(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Vec<Eterm>,
) -> String {
    let (out, result) = run_io_args(reg, fmt, build);
    assert!(result.is_ok(), "{fmt:?}: {result:?}");
    String::from_utf8(out).expect("the printer emits UTF-8")
}

/// `io:format(Fmt, [V])` for one float.
fn io_float(reg: &BifRegistry, fmt: &[u8], value: f64) -> String {
    io_text(reg, fmt, move |ctx, _| {
        vec![ctx.make_float(value).expect("a float fits the test heap")]
    })
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// Runs `erl -noshell -eval <statement>`; `None` (with a loud message) when the
/// oracle is unavailable, so the cross-checks skip rather than fail.
fn oracle(statement: &str) -> Option<Vec<String>> {
    match Command::new("erl")
        .args(["-noshell", "-eval", statement])
        .output()
    {
        Ok(out) if out.status.success() => Some(
            String::from_utf8(out.stdout)
                .expect("the oracle emits UTF-8")
                .lines()
                .map(str::to_string)
                .collect(),
        ),
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

/// The Erlang source literal for `value` — Rust's shortest round-trip spelling,
/// with the `.` an Erlang float literal requires (`5e-324` ⇒ `5.0e-324`). The
/// shortest digits parse back to exactly `value`, so the oracle sees the same
/// `f64`.
fn erl_literal(value: f64) -> String {
    let text = format!("{value:?}");
    match text.split_once('e') {
        Some((mantissa, exponent)) if !mantissa.contains('.') => {
            format!("{mantissa}.0e{exponent}")
        }
        _ => text,
    }
}

/// The oracle's `~w` rendering of each `value`, one per line.
///
/// The floats are passed as source literals, so this is the oracle's rendering of
/// the *same* `f64`s the Rust side renders.
fn oracle_w(values: &[f64]) -> Option<Vec<String>> {
    let literals: Vec<String> = values.iter().copied().map(erl_literal).collect();
    let statement = format!(
        "[io:format(\"~s~n\",[lists:flatten(io_lib:format(\"~w\",[V]))]) || V <- [{}]], halt().",
        literals.join(",")
    );
    oracle(&statement)
}

/// The oracle's `~p` rendering of each `value`, one per line.
fn oracle_p(values: &[f64]) -> Option<Vec<String>> {
    let literals: Vec<String> = values.iter().copied().map(erl_literal).collect();
    let statement = format!(
        "[io:format(\"~s~n\",[lists:flatten(io_lib:format(\"~p\",[V]))]) || V <- [{}]], halt().",
        literals.join(",")
    );
    oracle(&statement)
}

/// The oracle's `~e`/`~f`/`~g` renderings of each `value`, `|`-separated per line.
fn oracle_e_f_g(values: &[f64]) -> Option<Vec<String>> {
    let literals: Vec<String> = values.iter().copied().map(erl_literal).collect();
    let statement = format!(
        "[io:format(\"~s|~s|~s~n\",[lists:flatten(io_lib:format(\"~e\",[V])), \
         lists:flatten(io_lib:format(\"~f\",[V])), \
         lists:flatten(io_lib:format(\"~g\",[V]))]) || V <- [{}]], halt().",
        literals.join(",")
    );
    oracle(&statement)
}

/// A deterministic stream of raw `f64` bit patterns for the wide sweep: a 64-bit
/// LCG, filtering out the `inf`/`NaN` encodings the oracle cannot hold (OTP is
/// finite-float only — its `binary_to_term/1` rejects a `NEW_FLOAT_EXT` whose
/// exponent is `16#7FF`).
fn sweep_bits(count: usize) -> Vec<u64> {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bits = state;
        if (bits >> 52) & 0x7ff == 0x7ff {
            continue;
        }
        out.push(bits);
    }
    out
}

/// The oracle's `~w` rendering of each raw bit pattern, one per line — the
/// oracle decodes exactly the bit pattern, so no literal round-trip is involved.
fn oracle_w_bits(bits: &[u64]) -> Option<Vec<String>> {
    let encoded: Vec<String> = bits.iter().map(u64::to_string).collect();
    let statement = format!(
        "[begin <<F:64/float>> = <<B:64>>, io:format(\"~s~n\",[lists:flatten(io_lib:format(\"~w\",[F]))]) end || B <- [{}]], halt().",
        encoded.join(",")
    );
    oracle(&statement)
}

// ---------------------------------------------------------------------------
// Goldens
// ---------------------------------------------------------------------------

/// The magnitude/notation table: value, and the oracle's `~w`/`~p`/`~W`/`~P` text.
///
/// Produced by `erl -noshell -eval 'io:format("~p~n",[[{V,float_to_list(V,
/// [short])} || V <- F]])'` and confirmed to equal `io_lib:format("~w"/"~p"/"~W"/
/// "~P", …)` for every entry (`~W`/`~P` treat a float leaf identically).
const GOLDENS: &[(f64, &str)] = &[
    (0.0, "0.0"),
    (-0.0, "-0.0"),
    (1.0, "1.0"),
    (1.5, "1.5"),
    (0.1, "0.1"),
    (3.14159, "3.14159"),
    (-2.5, "-2.5"),
    (100.0, "100.0"),
    (100.5, "100.5"),
    // The notation rule: the decimal form only survives while it is not longer
    // than the scientific one.
    (1.0e-4, "0.0001"),
    (0.001, "0.001"),
    (1.2345e-5, "1.2345e-5"),
    (1.0e-5, "1.0e-5"),
    (1.0e-6, "1.0e-6"),
    (1.0e5, "1.0e5"),
    (1.0e9, "1.0e9"),
    (1.0e10, "1.0e10"),
    (1.0e16, "1.0e16"),
    (1.0e20, "1.0e20"),
    (1.0e100, "1.0e100"),
    (1.0e-300, "1.0e-300"),
    (123456789.5, "123456789.5"),
    (1234567895.0, "1234567895.0"),
    (12345678900.0, "12345678900.0"),
    (9.1e15, "9.1e15"),
    (1.5e-4, "1.5e-4"),
    (2.5e-7, "2.5e-7"),
    (3.141592653589793, "3.141592653589793"),
    (f64::MAX, "1.7976931348623157e308"),
    (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
    (5.0e-324, "5.0e-324"),
];

#[test]
fn plain_float_goldens_match_the_oracle() {
    let reg = default_registry();
    for &(value, expected) in GOLDENS {
        assert_eq!(io_float(&reg, b"~w", value), expected, "~w of {value:?}");
        assert_eq!(io_float(&reg, b"~p", value), expected, "~p of {value:?}");
        // `~W`/`~P` carry an explicit depth; a float leaf is depth-independent.
        for fmt in [&b"~W"[..], &b"~P"[..]] {
            let out = io_text(&reg, fmt, |ctx, _| {
                vec![
                    ctx.make_float(value).expect("fits"),
                    ctx.int(3).expect("fits"),
                ]
            });
            assert_eq!(
                out,
                expected,
                "{} of {value:?}",
                String::from_utf8_lossy(fmt)
            );
        }
    }
}

#[test]
fn the_live_oracle_agrees_on_the_golden_table() {
    let reg = default_registry();
    let values: Vec<f64> = GOLDENS.iter().map(|&(value, _)| value).collect();

    let Some(expected) = oracle_w(&values) else {
        return;
    };
    assert_eq!(expected.len(), values.len(), "the oracle printed every value");
    for (&value, want) in values.iter().zip(&expected) {
        assert_eq!(
            &io_float(&reg, b"~w", value),
            want,
            "~w of {value:?} (oracle golden)"
        );
    }

    let Some(expected) = oracle_p(&values) else {
        return;
    };
    for (&value, want) in values.iter().zip(&expected) {
        assert_eq!(
            &io_float(&reg, b"~p", value),
            want,
            "~p of {value:?} (oracle golden)"
        );
    }
}

#[test]
fn a_bit_pattern_sweep_agrees_with_the_live_oracle() {
    let reg = default_registry();
    let bits = sweep_bits(600);
    let Some(expected) = oracle_w_bits(&bits) else {
        return;
    };
    assert_eq!(expected.len(), bits.len(), "the oracle printed every value");
    let mut mismatches = Vec::new();
    for (&raw, want) in bits.iter().zip(&expected) {
        let value = f64::from_bits(raw);
        let got = io_float(&reg, b"~w", value);
        if &got != want {
            mismatches.push((value, got, want.clone()));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} bit patterns diverged from the oracle, first: {:?}",
        mismatches.len(),
        bits.len(),
        mismatches.first()
    );
}

/// `~w` of floats nested in containers pins the same leaf inside the list,
/// tuple and map renderers (the oracle's `io_lib:write1/4` recursion).
#[test]
fn floats_nested_in_containers_agree_with_the_oracle() {
    let reg = default_registry();

    let (out, result) = run_io_args(&reg, b"~w", |ctx, _| {
        let ten = ctx.make_float(1.0e10).expect("fits");
        let five = ctx.make_float(1.0e5).expect("fits");
        let small = ctx.make_float(1.0e-6).expect("fits");
        let tuple = ctx.tuple(&[five, small]).expect("a 2-tuple fits");
        let tail = ctx.cons(tuple, NIL).expect("fits");
        let list = ctx.cons(ten, tail).expect("fits");
        vec![list]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("UTF-8"), "[1.0e10,{1.0e5,1.0e-6}]");

    let (out, result) = run_io_args(&reg, b"~w", |ctx, _| {
        let key = ctx.atom("a").expect("a seeded atom");
        let value = ctx.make_float(1.0e10).expect("fits");
        let map = beam_codec::term::make_flatmap(ctx.heap(), &[(key, value)]).expect("fits");
        vec![map]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("UTF-8"), "#{a => 1.0e10}");

    // The oracle's own rendering of the same three terms.
    let Some(expected) = oracle(
        "io:format(\"~w~n~w~n\", [[1.0e10,{1.0e5,1.0e-6}], #{a => 1.0e10}]), halt().",
    ) else {
        return;
    };
    assert_eq!(
        expected,
        vec![
            "[1.0e10,{1.0e5,1.0e-6}]".to_string(),
            "#{a => 1.0e10}".to_string()
        ]
    );
}

/// The `~e`/`~f`/`~g` directives have their own field/precision handling
/// (`float_to_list/2` with `{scientific,N}`/`{decimals,N}`) and must be unchanged
/// by the plain-leaf fix. Goldens are the oracle's, re-derived live.
#[test]
fn the_e_f_g_directives_are_unchanged() {
    let reg = default_registry();
    // `~f` of the largest doubles renders far past 255 characters (316 for
    // `f64::MAX`): the `float_to_list/2` buffer bound is *not* on this path. The
    // extremes are pinned in `tests/float_directives.rs`.
    let values = [
        0.0f64,
        1.0,
        1.0e5,
        1.0e10,
        1.0e-5,
        1.0e-6,
        1.0e20,
        1.5e-4,
        123456789.5,
        3.141592653589793,
        -0.0,
        100.0,
        100.5,
        1.0e100,
        5.0e-324,
        1.0e-300,
    ];

    let Some(expected) = oracle_e_f_g(&values) else {
        return;
    };
    assert_eq!(expected.len(), values.len(), "the oracle printed every value");
    for (&value, want) in values.iter().zip(&expected) {
        let mut parts = want.split('|');
        let want_e = parts.next().expect("an `~e` field");
        let want_f = parts.next().expect("an `~f` field");
        let want_g = parts.next().expect("a `~g` field");
        assert_eq!(io_float(&reg, b"~e", value), want_e, "~e of {value:?}");
        assert_eq!(io_float(&reg, b"~f", value), want_f, "~f of {value:?}");
        assert_eq!(io_float(&reg, b"~g", value), want_g, "~g of {value:?}");
    }

    // Pinned goldens (the oracle's text), so the check still bites without `erl`.
    assert_eq!(io_float(&reg, b"~e", 1.0), "1.00000e+0");
    assert_eq!(io_float(&reg, b"~f", 1.0e5), "100000.000000");
    assert_eq!(io_float(&reg, b"~g", 1.0), "1.00000");
    assert_eq!(io_float(&reg, b"~g", 100.0), "100.000");
    assert_eq!(io_float(&reg, b"~g", 123456789.5), "1.23457e+8");
}

/// OTP holds no `inf`/`NaN` (`binary_to_term/1` refuses a `NEW_FLOAT_EXT` with
/// exponent `16#7FF`), so there is no oracle behaviour to match — but the printer
/// must render one without panicking (the BIF arithmetic that can fabricate an
/// `inf` is a separate, recorded gap).
#[test]
fn a_non_finite_float_does_not_panic() {
    let reg = default_registry();
    assert_eq!(io_float(&reg, b"~w", f64::INFINITY), "inf");
    assert_eq!(io_float(&reg, b"~w", f64::NEG_INFINITY), "-inf");
    assert_eq!(io_float(&reg, b"~p", f64::NAN), "NaN");
}

/// The `io_lib:format/2` entry point returns the same text as `io:format/2` after
/// `lists:flatten/1` — the float leaf is one element of the deep list.
#[test]
fn io_lib_format_flattens_to_the_same_float_text() {
    let reg = default_registry();
    for &(value, expected) in GOLDENS {
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let format = byte_list(&mut ctx, b"~w");
        let arg = ctx.make_float(value).expect("fits");
        let args = ctx.list(&[arg]).expect("a short arg list fits");
        let deep = reg
            .call("io_lib", "format", 2, &mut ctx, &[format, args])
            .expect("io_lib:format/2");
        let flat = reg
            .call("lists", "flatten", 1, &mut ctx, &[deep])
            .expect("lists:flatten/1");
        let mut text = String::new();
        let mut cur = flat;
        while is_list(cur) {
            let head = beam_codec::term::cons_head(cur).expect("a live cons cell");
            text.push(char::from(
                u8::try_from(signed_value(head)).expect("a byte-sized character"),
            ));
            cur = beam_codec::term::cons_tail(cur).expect("a live cons cell");
        }
        assert!(is_nil(cur), "a proper character list");
        assert_eq!(text, expected, "io_lib:format(\"~w\", […]) of {value:?}");
    }
}
