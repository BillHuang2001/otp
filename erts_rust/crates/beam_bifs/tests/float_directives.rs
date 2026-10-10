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
//! Integration tests: the `~f`/`~e`/`~g` float directives have **no 255-character
//! bound**.
//!
//! The `float_to_list/2` renderer ([`beam_bifs`]'s `numeric::render_float`) *is*
//! bounded by the C `char fbuf[256]` of `do_float_to_charbuf` (`erts/emulator/beam/
//! bif.c`), and that bound is reproduced faithfully (see `numeric.rs`'s tests). The
//! `io` float directives do **not** share it: `~e`/`~f`/`~g` are ports of
//! `io_lib_format:fwrite_e/5`/`fwrite_f/5`/`fwrite_g/5`
//! (`lib/stdlib/src/io_lib_format.erl`), which build their text through
//! `float_data/1` (= `float_to_list(F, [{scientific,20}])`) plus `float_man2/3`
//! and never touch the C float buffer — so they render, unbounded, whatever the
//! field/precision asks for. The oracle confirms it:
//!
//! ```text
//! $ erl -noshell -eval 'L=[1.7976931348623157e308,-1.7976931348623157e308,
//!     5.0e-324,1.0e308,123456789.123456789],
//!     R=[iolist_to_binary(io_lib:format("~f",[X]))||X<-L],
//!     io:format("lens=~p~n",[[erlang:byte_size(S)||S<-R]]), halt().'
//! lens=[316,317,8,316,16]
//! $ erl -noshell -eval 'F=1.7976931348623157e308,
//!     R=(catch iolist_to_binary(io_lib:format("~.100000f",[F]))),
//!     io:format("~p~n",[byte_size(R)]), halt().'
//! 100310
//! ```
//!
//! `~f` of `f64::MAX` therefore needs 316 characters (317 for the negative) at the
//! default precision, 610 at `~.300f`, and 1_000_310 at `~.1000000f` — none of them
//! `badarg`. Every golden below was read off the OTP 29.1.1 / erts-17.1 oracle and
//! the live cross-checks re-derive them (skipping loudly when no `erl` is on
//! `PATH`).
use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::prelude::*;
use std::process::Command;

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// An atom table seeded with the atoms the fixtures need.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "decimals", "scientific"]);
    atoms
}

/// The text of a flat character list (an Erlang string), asserting it is proper
/// and byte-sized.
fn char_list_text(term: Eterm) -> String {
    let mut out = String::new();
    let mut cur = term;
    while is_list(cur) {
        let head = beam_codec::term::cons_head(cur).expect("a live cons cell");
        out.push(char::from(
            u8::try_from(signed_value(head)).expect("a byte-sized character"),
        ));
        cur = beam_codec::term::cons_tail(cur).expect("a live cons cell");
    }
    assert!(is_nil(cur), "a proper character list");
    out
}

/// A flat proper list of the byte-sized integers of `bytes` (an Erlang string).
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let mut elems = Vec::with_capacity(bytes.len());
    for &b in bytes {
        elems.push(ctx.int(i64::from(b)).expect("a byte fits a small int"));
    }
    ctx.list(&elems).expect("a short list fits")
}

/// Runs `io:format(Fmt, [V])` over a `BytesSink`; `Err` when the BIF raises.
fn io_format_float(reg: &BifRegistry, fmt: &[u8], value: f64) -> Result<String, BeamError> {
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let result;
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, fmt);
        let arg = ctx.make_float(value).expect("a float fits the test heap");
        let args = ctx.list(&[arg]).expect("a short arg list fits");
        result = reg
            .call("io", "format", 2, &mut ctx, &[format, args])
            .map(|_| ());
    }
    result.map(|()| String::from_utf8(sink.contents().to_vec()).expect("UTF-8 output"))
}

/// `io:format(Fmt, [V])` text, asserting success.
fn text(reg: &BifRegistry, fmt: &[u8], value: f64) -> String {
    let out = io_format_float(reg, fmt, value);
    assert!(out.is_ok(), "{:?} of {value:?}: {out:?}", String::from_utf8_lossy(fmt));
    out.expect("checked above")
}

/// `lists:flatten(io_lib:format(Fmt, [V]))` text, asserting success — the same
/// rendering through the other entry point (a deep list of characters).
fn io_lib_text(reg: &BifRegistry, fmt: &[u8], value: f64) -> String {
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let format = byte_list(&mut ctx, fmt);
    let arg = ctx.make_float(value).expect("a float fits the test heap");
    let args = ctx.list(&[arg]).expect("a short arg list fits");
    let deep = reg
        .call("io_lib", "format", 2, &mut ctx, &[format, args])
        .expect("io_lib:format/2");
    let flat = reg
        .call("lists", "flatten", 1, &mut ctx, &[deep])
        .expect("lists:flatten/1");
    char_list_text(flat)
}

/// `erlang:float_to_list(V, Options)` — the **buffer-bounded** renderer of
/// `numeric::render_float`; `option` is `None` for `[]` and `Some((name, n))` for a
/// single `{name, n}` option.
fn float_to_list(
    reg: &BifRegistry,
    value: f64,
    option: Option<(&str, i64)>,
) -> Result<String, BeamError> {
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let arg = ctx.make_float(value).expect("a float fits the test heap");
    let options = match option {
        None => Vec::new(),
        Some((name, n)) => {
            let key = ctx.atom(name).expect("a seeded atom");
            let val = ctx.int(n).expect("a small int fits");
            vec![ctx.tuple(&[key, val]).expect("a 2-tuple fits")]
        }
    };
    let options = ctx.list(&options).expect("a short list fits");
    let rendered = reg.call("erlang", "float_to_list", 2, &mut ctx, &[arg, options])?;
    Ok(char_list_text(rendered))
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

/// The oracle's rendering of every `(format, value)` pair, in `formats` × `values`
/// order (the outer loop is the format): `ok|<text>` on success, `err` when the
/// oracle raises.
fn oracle_sweep(formats: &[&str], values: &[f64]) -> Option<Vec<String>> {
    let formats = formats
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(",");
    let values = values
        .iter()
        .copied()
        .map(erl_literal)
        .collect::<Vec<_>>()
        .join(",");
    let statement = format!(
        "[begin \
           R = (try iolist_to_binary(io_lib:format(F,[V])) catch _:_ -> error end), \
           io:format(\"~s~n\",[case R of error -> \"err\"; B -> <<\"ok|\",B/binary>> end]) \
         end || F <- [{formats}], V <- [{values}]], halt()."
    );
    oracle(&statement)
}

/// The Rust side of [`oracle_sweep`], in the same order and the same encoding.
fn rust_sweep(reg: &BifRegistry, formats: &[&str], values: &[f64]) -> Vec<String> {
    let mut out = Vec::with_capacity(formats.len() * values.len());
    for format in formats {
        for &value in values {
            out.push(match io_format_float(reg, format.as_bytes(), value) {
                Ok(text) => format!("ok|{text}"),
                Err(_) => "err".to_string(),
            });
        }
    }
    out
}

/// Runs a sweep on both sides and reports every divergence.
fn assert_sweep_agrees(reg: &BifRegistry, formats: &[&str], values: &[f64]) {
    let Some(expected) = oracle_sweep(formats, values) else {
        return;
    };
    let got = rust_sweep(reg, formats, values);
    assert_eq!(
        got.len(),
        expected.len(),
        "the sweep produced {} lines, the oracle {}",
        got.len(),
        expected.len()
    );
    let mut divergences = Vec::new();
    for (index, (got, want)) in got.iter().zip(&expected).enumerate() {
        let format = formats[index / values.len()];
        let value = values[index % values.len()];
        if got != want {
            divergences.push(format!("{format} of {value:?}: got {got:?}, oracle {want:?}"));
        }
    }
    assert!(
        divergences.is_empty(),
        "{} of {} cases diverged:\n{}",
        divergences.len(),
        got.len(),
        divergences.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Goldens
// ---------------------------------------------------------------------------

/// The 21 significant digits `float_to_list(F, [{scientific,20}])` yields for
/// `f64::MAX` (read off the oracle) — the leading digits of every `~f` rendering
/// of it; the decimal expansion from position 22 on is zeros.
const MAX_DIGITS: &str = "179769313486231570815";

/// `f64::MAX`'s `~f` rendering at the default precision: the 21 significant
/// digits, the remaining `309 - 21` integer digits as zeros, and `p` decimals.
fn max_fixed(decimals: usize) -> String {
    format!("{MAX_DIGITS}{}.{}", "0".repeat(288), "0".repeat(decimals))
}

#[test]
fn huge_fixed_renderings_are_untouched_by_the_float_to_list_buffer() {
    let reg = default_registry();

    // The objective's five extremes, at the default `~f` precision (6): lengths
    // `[316,317,8,316,16]` in the oracle.
    assert_eq!(text(&reg, b"~f", f64::MAX), max_fixed(6));
    assert_eq!(text(&reg, b"~f", f64::MAX).len(), 316);
    assert_eq!(
        text(&reg, b"~f", -f64::MAX),
        format!("-{}", max_fixed(6))
    );
    assert_eq!(text(&reg, b"~f", -f64::MAX).len(), 317);
    assert_eq!(text(&reg, b"~f", 5.0e-324), "0.000000");
    assert_eq!(text(&reg, b"~f", 5.0e-324).len(), 8);
    // `1.0e308`: the same 309 integer digits, different significant digits.
    let one_e308 = format!("100000000000000001098{}.000000", "0".repeat(288));
    assert_eq!(text(&reg, b"~f", 1.0e308), one_e308);
    assert_eq!(text(&reg, b"~f", 1.0e308).len(), 316);
    assert_eq!(text(&reg, b"~f", 123456789.123456789), "123456789.123457");
    assert_eq!(text(&reg, b"~f", 123456789.123456789).len(), 16);

    // An explicit precision adds decimals on top of the integer part, still with
    // no buffer in sight: 309 + 1 + p characters.
    assert_eq!(text(&reg, b"~.300f", f64::MAX), max_fixed(300));
    assert_eq!(text(&reg, b"~.300f", f64::MAX).len(), 610);
    assert_eq!(text(&reg, b"~.500f", f64::MAX), max_fixed(500));
    assert_eq!(text(&reg, b"~.500f", f64::MAX).len(), 810);
    assert_eq!(text(&reg, b"~.253f", f64::MAX).len(), 563);
    assert_eq!(text(&reg, b"~.254f", f64::MAX).len(), 564);
    assert_eq!(text(&reg, b"~.255f", f64::MAX).len(), 565);
    assert_eq!(text(&reg, b"~.1000f", f64::MAX), max_fixed(1000));
    assert_eq!(text(&reg, b"~.1000f", f64::MAX).len(), 1310);
}

/// The pathological end of the same family: a million decimals is 1_000_310
/// characters, which the oracle renders in full (the crate must too).
#[test]
fn a_million_decimals_is_rendered_in_full() {
    let reg = default_registry();
    let out = text(&reg, b"~.1000000f", f64::MAX);
    assert_eq!(out.len(), 1_000_310);
    assert_eq!(out, max_fixed(1_000_000));
    assert!(out.ends_with(&"0".repeat(64)), "the tail is decimals");
}

/// `~e`/`~g` grow with the precision the same way (`~e` has no exponent buffer and
/// `~g` delegates to `~f`/`~e`): `~.Ne` is `N + 1` significand characters plus the
/// `e+308` exponent.
#[test]
fn huge_scientific_and_general_renderings_are_not_capped() {
    let reg = default_registry();

    // `~.Ne` keeps the leading digit (unrounded for `f64::MAX`: `1797693…`) and
    // pads the remaining `N - 1 - 20` fraction digits with zeros.
    let e = text(&reg, b"~.1000e", f64::MAX);
    assert_eq!(
        e,
        format!(
            "1.{}{}e+308",
            &MAX_DIGITS[1..],
            "0".repeat((1000 - 1) - (MAX_DIGITS.len() - 1))
        )
    );
    assert_eq!(e.len(), 1000 + 1 + "e+308".len());
    assert_eq!(
        text(&reg, b"~.500e", f64::MAX).len(),
        500 + 1 + "e+308".len()
    );
    assert_eq!(text(&reg, b"~.248e", f64::MAX).len(), 254);
    assert_eq!(text(&reg, b"~.249e", f64::MAX).len(), 255);

    // `~g` of a large magnitude is the `~e` form; of a small one the `~f` form.
    assert_eq!(
        text(&reg, b"~.1000g", f64::MAX).len(),
        1000 + 1 + "e+308".len()
    );
    assert_eq!(text(&reg, b"~.300g", 1.0), format!("1.{}", "0".repeat(299)));
}

/// The `~e`/`~f`/`~g` directives of ordinary values are unchanged — the pinned
/// goldens (the oracle's text).
#[test]
fn ordinary_float_directives_are_unchanged() {
    let reg = default_registry();
    assert_eq!(text(&reg, b"~e", 1.0), "1.00000e+0");
    assert_eq!(text(&reg, b"~e", 1.5), "1.50000e+0");
    assert_eq!(text(&reg, b"~f", 1.0), "1.000000");
    assert_eq!(text(&reg, b"~f", 1.0e5), "100000.000000");
    assert_eq!(text(&reg, b"~f", 1.0e10), "10000000000.000000");
    assert_eq!(text(&reg, b"~f", 3.141592653589793), "3.141593");
    assert_eq!(text(&reg, b"~g", 1.0), "1.00000");
    assert_eq!(text(&reg, b"~g", 100.0), "100.000");
    assert_eq!(text(&reg, b"~g", 123456789.5), "1.23457e+8");
    assert_eq!(text(&reg, b"~12.4f", 1.567), "      1.5670");
    assert_eq!(text(&reg, b"~-12.4f", 1.567), "1.5670      ");
    // `~.0f`/`~.1e` are the oracle's `badarg`s (a precision below the minimum).
    assert!(matches!(
        io_format_float(&reg, b"~.0f", 1.0),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        io_format_float(&reg, b"~.1e", 1.0),
        Err(BeamError::BadArg)
    ));
}

/// The `io_lib:format/2` entry point returns a character list that flattens to the
/// same 316-character text — the huge rendering is not an `io:format`-only path.
#[test]
fn io_lib_format_flattens_to_the_same_huge_text() {
    let reg = default_registry();
    assert_eq!(io_lib_text(&reg, b"~f", f64::MAX), max_fixed(6));
    assert_eq!(io_lib_text(&reg, b"~.300f", f64::MAX).len(), 610);
}

// ---------------------------------------------------------------------------
// Live oracle cross-checks
// ---------------------------------------------------------------------------

/// The objective's probe, re-derived from the live oracle: the five extremes at
/// `~f` and at a span of explicit precisions/widths.
#[test]
fn the_live_oracle_agrees_on_the_extreme_fixed_renderings() {
    let reg = default_registry();
    let values = [
        f64::MAX,
        -f64::MAX,
        5.0e-324,
        1.0e308,
        123456789.123456789,
    ];
    let formats = [
        "~f",
        "~.1f",
        "~.6f",
        "~.100f",
        "~.253f",
        "~.254f",
        "~.255f",
        "~.300f",
        "~.500f",
        "~.1000f",
        "~.5000f",
        "~10f",
        "~40f",
        "~100f",
        "~400f",
        "~4000f",
        "~10.5f",
        "~40.30f",
        "~400.300f",
        "~-20.5f",
    ];
    assert_sweep_agrees(&reg, &formats, &values);
}

/// The extremes across the whole `~e`/`~f`/`~g` directive family, including the
/// `*` width/precision forms and the `badarg` edges.
#[test]
fn the_live_oracle_agrees_on_the_extremes_across_e_f_g() {
    let reg = default_registry();
    let values = [
        0.0,
        -0.0,
        1.0,
        1.5,
        1.0e10,
        1.0e-6,
        1.0e100,
        5.0e-324,
        f64::MIN_POSITIVE,
        f64::MAX,
        -f64::MAX,
        1.0e308,
    ];
    let formats = [
        "~e",
        "~f",
        "~g",
        "~.1e",
        "~.2e",
        "~.6e",
        "~.100e",
        "~.248e",
        "~.250e",
        "~.500e",
        "~.1f",
        "~.2f",
        "~.6f",
        "~.100f",
        "~.254f",
        "~.500f",
        "~.1g",
        "~.2g",
        "~.6g",
        "~.100g",
        "~.500g",
        "~10f",
        "~40f",
        "~-20.5f",
        "~20.5e",
        "~20.5g",
        "~0.0f",
        "~.0e",
        "~.0g",
    ];
    assert_sweep_agrees(&reg, &formats, &values);
}

/// The huge explicit widths and precisions, where a 255-character buffer would
/// show up as a `badarg` if it were on this path.
#[test]
fn the_live_oracle_agrees_on_huge_widths_and_precisions() {
    let reg = default_registry();
    let values = [f64::MAX, 1.0, 1.0e308, 123.456, 5.0e-324];
    let formats = [
        "~400f",
        "~4000f",
        "~1000.300f",
        "~5000.1000f",
        "~.300e",
        "~.500e",
        "~.1000e",
        "~.5000e",
        "~.300g",
        "~.1000g",
        "~.5000g",
        "~3000.2000f",
    ];
    assert_sweep_agrees(&reg, &formats, &values);
}

/// The probe from the bug report, verbatim, re-derived from the live oracle.
#[test]
fn the_reported_probe_agrees_with_the_oracle() {
    let reg = default_registry();
    let values = [
        f64::MAX,
        -f64::MAX,
        5.0e-324,
        1.0e308,
        123456789.123456789,
    ];
    let Some(lines) = oracle(
        "L=[1.7976931348623157e308,-1.7976931348623157e308,5.0e-324,1.0e308,123456789.123456789], \
         io:format(\"~s|~s|~s|~s|~s~n\",[iolist_to_binary(io_lib:format(\"~f\",[X]))||X<-L]), halt().",
    ) else {
        return;
    };
    let want = lines.join("");
    let got = values
        .iter()
        .map(|&value| text(&reg, b"~f", value))
        .collect::<Vec<_>>()
        .join("|");
    assert_eq!(got, want);
    assert_eq!(
        got.split('|').map(str::len).collect::<Vec<_>>(),
        vec![316, 317, 8, 316, 16]
    );
}

/// The two float paths are **different renderers**, and the crate reproduces each
/// one exactly: `io:format("~f", …)` prints the whole 316-character number, while
/// `erlang:float_to_list/2` on the very same value is the C `char fbuf[256]`'s
/// `badarg`. Both answers are oracle-cross-checked, so the unbounded directive
/// path can never be "fixed" by saddling it with the bounded one's limit.
#[test]
fn the_two_float_paths_differ_exactly_as_the_oracle_does() {
    let reg = default_registry();

    // `io:format("~f", [f64::MAX])` — unbounded.
    assert_eq!(text(&reg, b"~f", f64::MAX), max_fixed(6));

    // `erlang:float_to_list/2` — the `fbuf[256]` bound (and an unbounded `[]`).
    let cases: [Option<(&str, i64)>; 4] = [
        Some(("decimals", 6)),
        Some(("decimals", 300)),
        Some(("scientific", 300)),
        None,
    ];
    let got: Vec<String> = cases
        .iter()
        .map(|option| match float_to_list(&reg, f64::MAX, *option) {
            Ok(text) => format!("ok|{text}"),
            Err(_) => "err".to_string(),
        })
        .collect();

    // The oracle's own answers for the same four option lists.
    let Some(lines) = oracle(
        "F=1.7976931348623157e308, \
         [begin R=(catch float_to_list(F,Opts)), \
          io:format(\"~s~n\",[case is_list(R) of true -> \"ok|\"++R; false -> \"err\" end]) end \
          || Opts <- [[{decimals,6}],[{decimals,300}],[{scientific,300}],[]]], halt().",
    ) else {
        // Without the oracle, pin the shape: the three narrowings are `badarg`
        // and the default form is the 27-character `%.20e` rendering.
        assert_eq!(got, vec!["err", "err", "err", "ok|1.79769313486231570815e+308"]);
        return;
    };
    assert_eq!(got, lines);
    assert_eq!(
        got,
        vec!["err", "err", "err", "ok|1.79769313486231570815e+308"]
    );
}
