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

//! The `-eval` evaluator's **binary bit-string** semantics at the CLI boundary,
//! cross-checked against the system `erl`.
//!
//! The `eval` module's bit-string handling covers four families that an untyped
//! tree-walker can easily get subtly wrong, so each is pinned here end-to-end
//! (`env!("CARGO_BIN_EXE_beam_rt")` → CLI parser → evaluator → `~w` printer → exit code):
//!
//! * **little-endian, non-byte-aligned integer segments** — the byte-wise
//!   `integer_le_image`/`read_little_endian_bits` pair, where the trailing partial field
//!   enters *above* the whole bytes (both construction and matching);
//! * **`float` segments** (64-bit, 32-bit, `-little`, and the `float-native` pattern);
//! * **`utf8`/`utf16`/`utf32` segments** — construction (including the surrogate/range
//!   `badarg` rejections) and matching (a malformed code point is a **match failure**, a
//!   `case_clause`, never a `badarg`);
//! * **wider-than-64-bit integer patterns** and a **string literal** inside a binary
//!   pattern, plus the `unit:` specifier.
//!
//! # The wrapper (why a `fun`)
//!
//! `tools/differential` hand both runtimes the identical self-terminating `-eval` probe, so
//! this suite does too. The corpus's plain `try ({expr}) of V -> …` wrapper cannot be used
//! for a `case`/binary-pattern expression — `erl_eval` refuses to evaluate one in the `try`
//! body itself and exits 1 with empty stdout. Evaluating the expression **inside a `fun`**
//! and calling that fun in the `try` head is accepted by both runtimes, so every row
//! (pattern or not) uses the same wrapper and the two sides stay byte-comparable.
//!
//! Every expectation below is the value the **system `erl` on `$PATH`** produces; each row is
//! asserted three ways — the Rust runtime against the pinned golden, the oracle against the
//! same golden, and the two runtimes against each other — on trimmed stdout **and** exit
//! code. Per the shared harness, the oracle is discovered by **filesystem only** (never run
//! to test for existence) and a missing toolchain is a **loud skip**, never a silent pass.

use std::path::Path;
use std::process::Output;

mod support;

/// The probe wrapper: evaluate the expression inside a `fun`, then call it.
///
/// The differential corpus's own wrapper substitutes the expression straight into the `try`
/// head; a `case`/binary-pattern expression is rejected there by `erl_eval`, hence the `fun`.
const WRAPPER: &str = concat!(
    "try (fun() -> {} end)() of V -> io:format(\"~w~n\",[V]), halt(0) ",
    "catch C:R -> io:format(\"exception ~w ~w~n\",[C,R]), halt(1) end."
);

/// Wrap `expr` in [`WRAPPER`] (the template holds a single `{}` slot; the expression itself
/// may contain braces, so only the first occurrence is substituted).
fn wrap(expr: &str) -> String {
    WRAPPER.replacen("{}", expr, 1)
}

/// One pinned row: the plain Erlang expression, the exact stdout the system `erl` prints
/// (trailing newline stripped) and the process exit code.
struct Row {
    /// The bare Erlang expression (`-eval`'s operand, before wrapping).
    expr: &'static str,
    /// The exact trimmed stdout of the oracle.
    stdout: &'static str,
    /// The oracle's exit code (`halt/1`).
    exit: i32,
}

/// `(trimmed_stdout, exit, trimmed_stderr)`, or `None` when the child was killed by a signal
/// (so no exit code exists).
fn observe(output: &Output) -> Option<(String, i32, String)> {
    Some((
        String::from_utf8_lossy(&output.stdout).trim_end().to_string(),
        output.status.code()?,
        String::from_utf8_lossy(&output.stderr).trim_end().to_string(),
    ))
}

/// Run every row of one family through **both** runtimes and assert the three equalities.
///
/// The Rust runtime is `env!("CARGO_BIN_EXE_beam_rt")` with the differential argv
/// (`-noshell -eval <wrapped>`); the oracle is `erl -noshell -eval <wrapped> -s init stop`,
/// both bounded by [`support::PROBE_TIMEOUT`]. An oracle that cannot answer within the bound
/// is a **loud** per-row skip (the golden assertion has already run); a missing `erl` skips
/// the whole family loudly.
fn cross_check(family: &str, rows: &[Row]) {
    let Some(oracle) = support::require_oracle(family) else {
        return;
    };

    // `require_oracle` proves only that `erl` **exists** (a filesystem question, deliberately
    // never answered by running it). A present-but-unusable oracle would quietly turn every
    // row below into a per-row "could not answer" skip — a suite that passes without a single
    // cross-check. Prove the tool actually *answers* first, and skip the family loudly if it
    // does not (a toolchain problem is never a failure and never a silent pass).
    if support::oracle_eval(&oracle, None, "io:format(\"~w\",[1+1]), halt(0).").as_deref() != Some("2")
    {
        support::skip_loudly(family, "the system `erl` cannot answer a trivial probe");
        return;
    }

    let binary = Path::new(env!("CARGO_BIN_EXE_beam_rt"));

    let mut agreed = 0usize;
    let mut skipped = 0usize;

    for row in rows {
        let wrapped = wrap(row.expr);

        // --- this runtime, through the built binary and the CLI `-eval` route -----------
        let output = support::run_bounded(binary, &["-noshell", "-eval", wrapped.as_str()])
            .unwrap_or_else(|| {
                panic!("beam_rt could not answer {:?} within the bound", row.expr)
            });
        let (stdout, exit, stderr) =
            observe(&output).unwrap_or_else(|| panic!("beam_rt was killed by a signal: {:?}", row.expr));
        assert!(
            stderr.is_empty(),
            "{family}: {:?}: unexpected stderr {stderr:?}",
            row.expr
        );
        assert_eq!(
            stdout, row.stdout,
            "{family}: {:?}: beam_rt stdout (stderr {stderr:?})",
            row.expr
        );
        assert_eq!(
            exit, row.exit,
            "{family}: {:?}: beam_rt exit (stderr {stderr:?})",
            row.expr
        );

        // --- the system `erl` oracle ---------------------------------------------------
        let oracle_output = support::run_bounded(
            &oracle,
            &["-noshell", "-eval", wrapped.as_str(), "-s", "init", "stop"],
        );
        let Some(oracle_output) = oracle_output else {
            eprintln!(
                "SKIP {family}: {:?}: the oracle probe could not answer within the bound",
                row.expr
            );
            skipped += 1;
            continue;
        };
        let Some((oracle_stdout, oracle_exit, _)) = observe(&oracle_output) else {
            eprintln!("SKIP {family}: {:?}: the oracle exited abnormally", row.expr);
            skipped += 1;
            continue;
        };
        assert_eq!(
            oracle_stdout, row.stdout,
            "{family}: {:?}: the oracle disagrees with the pinned golden",
            row.expr
        );
        assert_eq!(
            oracle_exit, row.exit,
            "{family}: {:?}: the oracle exit disagrees with the pinned golden",
            row.expr
        );
        assert_eq!(
            stdout, oracle_stdout,
            "{family}: {:?}: beam_rt stdout differs from the oracle",
            row.expr
        );
        assert_eq!(
            exit, oracle_exit,
            "{family}: {:?}: beam_rt exit differs from the oracle",
            row.expr
        );
        agreed += 1;
    }

    println!(
        "{family}: {agreed}/{} rows agree with the system erl ({skipped} oracle probes skipped)",
        rows.len()
    );
}

/// Little-endian, **non-byte-aligned** integer segments — construction (the trailing partial
/// field enters *above* the whole bytes) and the matching inverse.
#[test]
fn little_endian_integer_segments() {
    let rows: &[Row] = &[
        // Construction.
        Row {
            expr: "<<-300:12/little>>",
            stdout: "<<212,14:4>>",
            exit: 0,
        },
        Row {
            expr: "<<16#1234:12/little>>",
            stdout: "<<52,2:4>>",
            exit: 0,
        },
        Row {
            expr: "<<256:9/little>>",
            stdout: "<<0,1:1>>",
            exit: 0,
        },
        Row {
            expr: "<<(1 bsl 60):60/little>>",
            stdout: "<<0,0,0,0,0,0,0,0:4>>",
            exit: 0,
        },
        // No-regress controls: a byte-aligned little-endian field and a wide one.
        Row {
            expr: "<<255:12/little>>",
            stdout: "<<255,0:4>>",
            exit: 0,
        },
        Row {
            expr: "<<(1 bsl 64):72/little>>",
            stdout: "<<0,0,0,0,0,0,0,0,1>>",
            exit: 0,
        },
        // Matching is the inverse of construction.
        Row {
            expr: "case <<212,14:4>> of <<A:12/little>> -> A end",
            stdout: "3796",
            exit: 0,
        },
        Row {
            expr: "case <<0,1:1>> of <<A:9/little>> -> A end",
            stdout: "256",
            exit: 0,
        },
        Row {
            expr: "case <<52,2:4>> of <<A:12/little>> -> A end",
            stdout: "564",
            exit: 0,
        },
    ];
    cross_check("little_endian_integer_segments", rows);
}

/// `float` segments, `utf8`/`utf16`/`utf32` construction (including the range/surrogate
/// `badarg` rejections) and the `utf`/`float` matching forms.
#[test]
fn float_and_utf_segments() {
    let rows: &[Row] = &[
        // --- float construction ---------------------------------------------------------
        Row {
            expr: "<<1.5/float>>",
            stdout: "<<63,248,0,0,0,0,0,0>>",
            exit: 0,
        },
        Row {
            expr: "<<1.5/float-little>>",
            stdout: "<<0,0,0,0,0,0,248,63>>",
            exit: 0,
        },
        Row {
            expr: "<<1.5:32/float>>",
            stdout: "<<63,192,0,0>>",
            exit: 0,
        },
        Row {
            expr: "<<1.5:32/float-little>>",
            stdout: "<<0,0,192,63>>",
            exit: 0,
        },
        Row {
            expr: "<<1.0e40:64/float>>",
            stdout: "<<72,61,99,41,241,195,92,165>>",
            exit: 0,
        },
        Row {
            expr: "<<(1 bsl 64):64/float>>",
            stdout: "<<67,240,0,0,0,0,0,0>>",
            exit: 0,
        },
        Row {
            expr: "bit_size(<<1.0/float>>)",
            stdout: "64",
            exit: 0,
        },
        // --- utf construction -----------------------------------------------------------
        Row {
            expr: "<<16#41/utf16>>",
            stdout: "<<0,65>>",
            exit: 0,
        },
        Row {
            expr: "<<16#41/utf16-little>>",
            stdout: "<<65,0>>",
            exit: 0,
        },
        Row {
            expr: "<<1234/utf32>>",
            stdout: "<<0,0,4,210>>",
            exit: 0,
        },
        Row {
            expr: "<<1234/utf32-little>>",
            stdout: "<<210,4,0,0>>",
            exit: 0,
        },
        Row {
            expr: "<<16#10FFFF/utf16>>",
            stdout: "<<219,255,223,255>>",
            exit: 0,
        },
        Row {
            expr: "<<16#10FFFF/utf32>>",
            stdout: "<<0,16,255,255>>",
            exit: 0,
        },
        // Out of range / surrogate / negative ⇒ `badarg` at construction.
        Row {
            expr: "<<16#D800/utf16>>",
            stdout: "exception error badarg",
            exit: 1,
        },
        Row {
            expr: "<<16#110000/utf32>>",
            stdout: "exception error badarg",
            exit: 1,
        },
        Row {
            expr: "<<-1/utf8>>",
            stdout: "exception error badarg",
            exit: 1,
        },
        Row {
            expr: "<<16#110000/utf8>>",
            stdout: "exception error badarg",
            exit: 1,
        },
        // --- utf / float matching -------------------------------------------------------
        Row {
            expr: "case <<16#41/utf8>> of <<C/utf8>> -> C end",
            stdout: "65",
            exit: 0,
        },
        Row {
            expr: "case <<16#E0,16#A0,16#80>> of <<C/utf8>> -> C end",
            stdout: "2048",
            exit: 0,
        },
        Row {
            expr: "case <<16#10FFFF/utf16>> of <<C/utf16>> -> C end",
            stdout: "1114111",
            exit: 0,
        },
        Row {
            expr: "case <<16#FF,16#10,0,0>> of <<C/utf32-little>> -> C end",
            stdout: "4351",
            exit: 0,
        },
        // A malformed code point is a **match failure** (`case_clause`), never a `badarg`.
        Row {
            expr: "case <<16#D8,0>> of <<C/utf16>> -> C end",
            stdout: "exception error {case_clause,<<216,0>>}",
            exit: 1,
        },
        Row {
            expr: "case <<0,0,16#10,16#FF>> of <<C/utf32-little>> -> C end",
            stdout: "exception error {case_clause,<<0,0,16,255>>}",
            exit: 1,
        },
        Row {
            expr: "case <<1.5:32/float>> of <<F:32/float>> -> F end",
            stdout: "1.5",
            exit: 0,
        },
        Row {
            expr: "case <<1.5:64/float>> of <<F:64/float-native>> -> F end",
            stdout: "3.13984e-319",
            exit: 0,
        },
    ];
    cross_check("float_and_utf_segments", rows);
}

/// Integer patterns wider than 64 bits (unsigned, signed, little-endian and a too-short
/// binary, which is a **match failure** rather than an error).
#[test]
fn wide_integer_pattern_segments() {
    let rows: &[Row] = &[
        Row {
            expr: "case <<255,255,255,255,255,255,255,255>> of <<A:64>> -> A end",
            stdout: "18446744073709551615",
            exit: 0,
        },
        Row {
            expr: "case <<255,255,255,255,255,255,255,255>> of <<A:64/signed>> -> A end",
            stdout: "-1",
            exit: 0,
        },
        Row {
            expr: "case <<1,2,3,4,5,6,7,8>> of <<A:64/little>> -> A end",
            stdout: "578437695752307201",
            exit: 0,
        },
        Row {
            expr: "case <<255,255,255,255,255,255,255,255,255>> of <<A:72>> -> A end",
            stdout: "4722366482869645213695",
            exit: 0,
        },
        Row {
            expr: "case <<1,2,3,4,5,6,7,8,9>> of <<A:72/signed-little>> -> A end",
            stdout: "166599134359138271745",
            exit: 0,
        },
        // 65 bits requested from 72 available: the width does not divide evenly, so the
        // clause simply does not match.
        Row {
            expr: "case <<255,255,255,255,255,255,255,255,255>> of <<A:65>> -> A end",
            stdout: "exception error {case_clause,<<255,255,255,255,255,255,255,255,255>>}",
            exit: 1,
        },
    ];
    cross_check("wide_integer_pattern_segments", rows);
}

/// A **string literal** inside a binary pattern expands to one 8-bit segment per character,
/// at any position of the pattern.
#[test]
fn string_pattern_segments() {
    let rows: &[Row] = &[
        Row {
            expr: "case <<\"abc\">> of <<\"abc\">> -> yes; _ -> no end",
            stdout: "yes",
            exit: 0,
        },
        Row {
            expr: "case <<1,\"abc\">> of <<X:8,\"abc\">> -> X end",
            stdout: "1",
            exit: 0,
        },
        Row {
            expr: "case <<\"abc\",1>> of <<\"abc\", X:8>> -> X end",
            stdout: "1",
            exit: 0,
        },
    ];
    cross_check("string_pattern_segments", rows);
}

/// The `unit:` specifier (default 1; `unit:8` = bytes) on construction and matching, in both
/// endiannesses.
#[test]
fn unit_specifiers() {
    let rows: &[Row] = &[
        Row {
            expr: "case <<16#12,16#34>> of <<A:2/unit:8>> -> A end",
            stdout: "4660",
            exit: 0,
        },
        Row {
            expr: "<<1:2/unit:8>>",
            stdout: "<<0,1>>",
            exit: 0,
        },
        Row {
            expr: "<<256:2/unit:8>>",
            stdout: "<<1,0>>",
            exit: 0,
        },
        Row {
            expr: "<<16#1234:2/little-unit:8>>",
            stdout: "<<52,18>>",
            exit: 0,
        },
    ];
    cross_check("unit_specifiers", rows);
}
