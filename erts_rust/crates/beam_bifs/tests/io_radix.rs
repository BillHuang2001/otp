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

//! Integration tests for the `~+`/`~#` **base-prefix** directives and for the
//! **arbitrary-precision** radix directives (`~b`/`~B`/`~x`/`~X`) of the shared
//! `io`/`io_lib` format engine (public API only, driven through [`BifRegistry`]).
//!
//! `~+`/`~#` render an integer in the base given by the directive's precision
//! (default 10) carrying the `Base#` prefix that base implies — lowercase digits
//! for `~+`, uppercase for `~#` (`io_lib_format:control_small($+…)`/`($#…)` →
//! `prefixed_integer/7`). `~b`/`~B`/`~x`/`~X` render the same integer in
//! `~.<base>…` (default 10); all six accept a **bignum** and print it in full,
//! through the crate's single arbitrary-precision renderer
//! (`numeric::integer_digits`, the one `integer_to_list/2` uses).
//!
//! Every expectation is the oracle's own flattened output:
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n",[lists:flatten(io_lib:format(F,A))]).' -s init stop
//! ```
//!
//! on erts-17.1 — captured with `~w` so the character **codes** (not the printable
//! rendering) are compared. [`the_pinned_goldens_match_the_registry`] pins them
//! against this crate; [`the_pinned_goldens_match_the_live_oracle`] re-derives all
//! of them live from the system `erl` and SKIPS LOUDLY when no oracle is available
//! (the crate's established pattern), so the two together assert *registry ==
//! oracle*.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{is_list, is_small, signed_value, AtomTable, Eterm, NIL};

/// The limbs of `123456789012345678901234567890` (`0x18EE90FF6C373E0EE4E3F0AD2`),
/// least significant first — the bignum every overflow case uses.
const BIG30: &[u64] = &[0xc373_e0ee_4e3f_0ad2, 0x18ee_90ff_6];

/// The decimal literal of [`BIG30`] (the oracle's source text).
const BIG30_SRC: &str = "123456789012345678901234567890";

/// The flattened `~.2#` rendering of **minus** [`BIG30`] on the oracle:
/// `io_lib:format("~.2#", [-123456789012345678901234567890])`.
const BIG30_BIN: &str = "-2#1100011101110100100001111111101101100001101110011111000001110111001001110001111110000101011010010";

/// One Erlang argument, built on the Rust side as a term and on the oracle side as
/// Erlang source text.
#[derive(Clone, Copy)]
enum Arg {
    /// A small integer.
    Small(i64),
    /// A float.
    Float(f64),
    /// An arbitrary-precision integer: `(negative, limbs, decimal source)`.
    Big(bool, &'static [u64], &'static str),
    /// A character list (an Erlang string).
    Str(&'static str),
}

/// The oracle's rendering of a case: the flattened text, or `badarg`.
#[derive(Clone, Copy)]
enum Want {
    /// The flattened character list `lists:flatten(io_lib:format(F, A))`.
    Text(&'static str),
    /// `io_lib:format(F, A)` raises `error:badarg`.
    BadArg,
}

/// One `io_lib:format(Format, Args)` case.
struct Case {
    /// The format string (ASCII, so it is also valid Erlang source).
    format: &'static str,
    /// The argument list.
    args: &'static [Arg],
    /// The oracle's result.
    want: Want,
}

/// Every case: the new `~+`/`~#` directives, the previously-working radix
/// directives, and the `badarg` set (which must stay exactly the oracle's).
const CASES: &[Case] = &[
    // ---- ~+ / ~# : the base prefix, default base 10 -----------------------------
    Case { format: "~+", args: &[Arg::Small(255)], want: Want::Text("10#255") },
    Case { format: "~#", args: &[Arg::Small(255)], want: Want::Text("10#255") },
    Case { format: "~+", args: &[Arg::Small(0)], want: Want::Text("10#0") },
    Case { format: "~+", args: &[Arg::Small(-255)], want: Want::Text("-10#255") },
    Case { format: "~#", args: &[Arg::Small(-1)], want: Want::Text("-10#1") },
    Case { format: "~.16+", args: &[Arg::Small(255)], want: Want::Text("16#ff") },
    Case { format: "~.16#", args: &[Arg::Small(255)], want: Want::Text("16#FF") },
    Case { format: "~.2+", args: &[Arg::Small(5)], want: Want::Text("2#101") },
    Case { format: "~.2#", args: &[Arg::Small(-5)], want: Want::Text("-2#101") },
    Case { format: "~.36+", args: &[Arg::Small(255)], want: Want::Text("36#73") },
    Case { format: "~.36#", args: &[Arg::Small(1295)], want: Want::Text("36#ZZ") },
    Case { format: "~.36+", args: &[Arg::Small(0)], want: Want::Text("36#0") },
    // Field width / adjust / pad, and the `term/6` truncation rule.
    Case { format: "~8+", args: &[Arg::Small(255)], want: Want::Text("  10#255") },
    Case { format: "~-8+", args: &[Arg::Small(255)], want: Want::Text("10#255  ") },
    Case { format: "~8.16+", args: &[Arg::Small(255)], want: Want::Text("   16#ff") },
    Case { format: "~-8.16#", args: &[Arg::Small(255)], want: Want::Text("16#FF   ") },
    Case { format: "~.16.*#", args: &[Arg::Small(255), Arg::Small(0x78)], want: Want::Text("16#78") },
    Case { format: "~3+", args: &[Arg::Small(255)], want: Want::Text("***") },
    Case { format: "~6+", args: &[Arg::Small(255)], want: Want::Text("10#255") },
    Case { format: "~4.16#", args: &[Arg::Small(255)], want: Want::Text("****") },
    // A character following the control is a literal.
    Case { format: "~+#", args: &[Arg::Small(255)], want: Want::Text("10#255#") },
    Case { format: "~.16#+", args: &[Arg::Small(255)], want: Want::Text("16#FF+") },
    // A float argument is `badarg`; `~+`/`~#` take exactly one argument.
    Case { format: "~+", args: &[Arg::Float(1.0)], want: Want::BadArg },
    Case { format: "~#", args: &[Arg::Float(1.0)], want: Want::BadArg },
    Case { format: "~+", args: &[Arg::Small(255), Arg::Small(1)], want: Want::BadArg },
    // ---- the bad-base set (the oracle's `2 =< Base =< 36` guard) ----------------
    Case { format: "~.1b", args: &[Arg::Small(255)], want: Want::BadArg },
    Case { format: "~.0b", args: &[Arg::Small(255)], want: Want::BadArg },
    Case { format: "~.37b", args: &[Arg::Small(255)], want: Want::BadArg },
    Case { format: "~.1+", args: &[Arg::Small(255)], want: Want::BadArg },
    Case { format: "~.0#", args: &[Arg::Small(255)], want: Want::BadArg },
    Case { format: "~.40#", args: &[Arg::Small(255)], want: Want::BadArg },
    Case { format: "~.37+", args: &[Arg::Small(255)], want: Want::BadArg },
    // ---- bignums: every radix directive prints them in full ---------------------
    Case { format: "~b", args: &[Arg::Big(false, BIG30, BIG30_SRC)], want: Want::Text(BIG30_SRC) },
    Case {
        format: "~B",
        args: &[Arg::Big(true, BIG30, BIG30_SRC)],
        want: Want::Text("-123456789012345678901234567890"),
    },
    Case {
        format: "~.16b",
        args: &[Arg::Big(false, BIG30, BIG30_SRC)],
        want: Want::Text("18ee90ff6c373e0ee4e3f0ad2"),
    },
    Case {
        format: "~.16B",
        args: &[Arg::Big(false, BIG30, BIG30_SRC)],
        want: Want::Text("18EE90FF6C373E0EE4E3F0AD2"),
    },
    Case {
        format: "~.16#",
        args: &[Arg::Big(false, BIG30, BIG30_SRC)],
        want: Want::Text("16#18EE90FF6C373E0EE4E3F0AD2"),
    },
    Case {
        format: "~.16+",
        args: &[Arg::Big(true, BIG30, BIG30_SRC)],
        want: Want::Text("-16#18ee90ff6c373e0ee4e3f0ad2"),
    },
    Case {
        format: "~.16#",
        args: &[Arg::Big(true, BIG30, BIG30_SRC)],
        want: Want::Text("-16#18EE90FF6C373E0EE4E3F0AD2"),
    },
    Case {
        format: "~.35+",
        args: &[Arg::Big(true, BIG30, BIG30_SRC)],
        want: Want::Text("-35#jukwca2sruj23ym3kt0"),
    },
    Case { format: "~.2#", args: &[Arg::Big(true, BIG30, BIG30_SRC)], want: Want::Text(BIG30_BIN) },
    // A bignum longer than the field truncates to exactly F `*` characters.
    Case {
        format: "~8.16b",
        args: &[Arg::Big(false, BIG30, BIG30_SRC)],
        want: Want::Text("********"),
    },
    Case {
        format: "~40b",
        args: &[Arg::Big(false, BIG30, BIG30_SRC)],
        want: Want::Text("          123456789012345678901234567890"),
    },
    Case {
        format: "~.16x",
        args: &[Arg::Big(false, BIG30, BIG30_SRC), Arg::Str("#")],
        want: Want::Text("#18ee90ff6c373e0ee4e3f0ad2"),
    },
    Case {
        format: "~.16X",
        args: &[Arg::Big(false, BIG30, BIG30_SRC), Arg::Str("#")],
        want: Want::Text("#18EE90FF6C373E0EE4E3F0AD2"),
    },
    Case {
        format: "~.16x",
        args: &[Arg::Big(true, BIG30, BIG30_SRC), Arg::Str("#")],
        want: Want::Text("-#18ee90ff6c373e0ee4e3f0ad2"),
    },
    // ---- the previously-working radix cases (unchanged) -------------------------
    Case { format: "~b", args: &[Arg::Small(255)], want: Want::Text("255") },
    Case { format: "~B", args: &[Arg::Small(255)], want: Want::Text("255") },
    Case { format: "~.16b", args: &[Arg::Small(255)], want: Want::Text("ff") },
    Case { format: "~.16B", args: &[Arg::Small(255)], want: Want::Text("FF") },
    Case { format: "~.16b", args: &[Arg::Small(-255)], want: Want::Text("-ff") },
    Case { format: "~.16B", args: &[Arg::Small(-255)], want: Want::Text("-FF") },
    Case { format: "~.2B", args: &[Arg::Small(5)], want: Want::Text("101") },
    Case { format: "~.2b", args: &[Arg::Small(-5)], want: Want::Text("-101") },
    Case { format: "~8.16b", args: &[Arg::Small(255)], want: Want::Text("      ff") },
    Case { format: "~-8.16b", args: &[Arg::Small(255)], want: Want::Text("ff      ") },
    Case { format: "~x", args: &[Arg::Small(255), Arg::Str("#")], want: Want::Text("#255") },
    Case { format: "~X", args: &[Arg::Small(255), Arg::Str("#")], want: Want::Text("#255") },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Str("#")], want: Want::Text("#ff") },
    Case { format: "~.16X", args: &[Arg::Small(255), Arg::Str("#")], want: Want::Text("#FF") },
    Case { format: "~x", args: &[Arg::Small(255), Arg::Str("0x")], want: Want::Text("0x255") },
    Case { format: "~.10x", args: &[Arg::Small(255), Arg::Str("abc")], want: Want::Text("abc255") },
    Case {
        format: "~x",
        args: &[Arg::Small(-255), Arg::Str("0x")],
        want: Want::Text("-0x255"),
    },
];

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok"]);
    atoms
}

/// A proper list of the byte-sized integers of `text` (an Erlang string).
fn byte_list(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
    let elems: Vec<Eterm> = text
        .bytes()
        .map(|b| ctx.int(i64::from(b)).expect("a character code fits"))
        .collect();
    ctx.list(&elems).expect("a short list fits")
}

/// Materializes an [`Arg`] on `ctx`'s heap.
fn build_arg(ctx: &mut BifContext<'_>, arg: &Arg) -> Eterm {
    match arg {
        Arg::Small(value) => ctx.int(*value).expect("a small int fits"),
        Arg::Float(value) => ctx.make_float(*value).expect("a float fits"),
        Arg::Big(negative, limbs, _) => {
            beam_codec::term::make_big(ctx.heap(), *negative, limbs).expect("a test bignum")
        }
        Arg::Str(text) => byte_list(ctx, text),
    }
}

/// Flattens an `io_lib:format` deep list (a nested list of character codes) to its
/// text. Every element must be a byte-sized integer — the cases are all ASCII.
fn flat_text(term: Eterm) -> String {
    let mut out = String::new();
    let mut stack = vec![term];
    while let Some(item) = stack.pop() {
        if is_small(item) {
            let value = signed_value(item);
            assert!(
                (0..=255).contains(&value),
                "io_lib:format returned the non-byte character code {value}"
            );
            out.push(value as u8 as char);
            continue;
        }
        let mut cursor = item;
        let mut push = Vec::new();
        while cursor != NIL {
            assert!(is_list(cursor), "io_lib:format returned an improper list");
            push.push(
                beam_codec::term::cons_head(cursor).expect("a cons cell has a head"),
            );
            cursor = beam_codec::term::cons_tail(cursor).expect("a cons cell has a tail");
        }
        // Preserve order.
        while let Some(element) = push.pop() {
            stack.push(element);
        }
    }
    out
}

/// Renders one case through `io_lib:format/2`.
fn render(ctx: &mut BifContext<'_>, reg: &BifRegistry, case: &Case) -> Result<String> {
    let format = byte_list(ctx, case.format);
    let mut elems = Vec::with_capacity(case.args.len());
    for arg in case.args {
        elems.push(build_arg(ctx, arg));
    }
    let args = ctx.list(&elems).expect("a short argument list fits");
    let out = reg.call("io_lib", "format", 2, ctx, &[format, args])?;
    Ok(flat_text(out))
}

/// Describes a case for the assertion messages.
fn describe(case: &Case) -> String {
    let args: Vec<String> = case
        .args
        .iter()
        .map(|arg| match arg {
            Arg::Small(value) => value.to_string(),
            // `{:?}` keeps the `.0` Erlang needs for a float literal.
            Arg::Float(value) => format!("{value:?}"),
            Arg::Big(negative, _, src) => {
                format!("{}{src}", if *negative { "-" } else { "" })
            }
            Arg::Str(text) => format!("{text:?}"),
        })
        .collect();
    format!("io_lib:format({:?}, [{}])", case.format, args.join(", "))
}

/// The pinned goldens are what this crate's registry renders.
#[test]
fn the_pinned_goldens_match_the_registry() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    for case in CASES {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let got = render(&mut ctx, &reg, case);
        match (got, case.want) {
            (Ok(text), Want::Text(want)) => {
                assert_eq!(text, want, "{} does not match the pinned golden", describe(case));
            }
            (Err(BeamError::BadArg), Want::BadArg) => {}
            (Ok(text), Want::BadArg) => {
                panic!("{} should be badarg, rendered {text:?}", describe(case));
            }
            (Err(other), _) => {
                panic!("{} unexpectedly failed: {other:?}", describe(case));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` and the sibling test files use.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs io_radix] $ERTS_ORACLE='{}' is not an existing executable \
             — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// The Erlang source of an argument list.
fn args_source(args: &[Arg]) -> String {
    let parts: Vec<String> = args
        .iter()
        .map(|arg| match arg {
            Arg::Small(value) => value.to_string(),
            // `{:?}` keeps the `.0` Erlang needs for a float literal.
            Arg::Float(value) => format!("{value:?}"),
            Arg::Big(negative, _, src) => {
                format!("{}{src}", if *negative { "-" } else { "" })
            }
            // An Erlang string literal — every case is ASCII with no escapes.
            Arg::Str(text) => format!("{text:?}"),
        })
        .collect();
    format!("[{}]", parts.join(","))
}

/// Runs one `erl -noshell -eval <eval> -s init stop` and returns its stdout lines.
fn run_erl(oracle: &Path, eval: &str) -> Vec<String> {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"));
    stdout.lines().map(str::to_owned).collect()
}

/// Every pinned golden is the live oracle's own output. Skips loudly when no
/// oracle is available.
#[test]
fn the_pinned_goldens_match_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=========== beam_bifs io_radix: live oracle SKIPPED ===========");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("==============================================================");
        eprintln!();
        return;
    };

    // One session for the whole table: `lists:flatten` each case and print the
    // character codes with `~w` (which never re-flows), or the atom `badarg`.
    let cases: Vec<String> = CASES
        .iter()
        .map(|case| format!("{{{:?},{}}}", case.format, args_source(case.args)))
        .collect();
    let eval = format!(
        "lists:foreach(fun({{F,A}}) -> R = try lists:flatten(io_lib:format(F,A)) \
         of V -> {{ok,V}} catch _:_ -> badarg end, io:format(\"~w~n\",[R]) end, [{}]).",
        cases.join(",")
    );
    let lines = run_erl(&oracle, &eval);
    assert_eq!(
        lines.len(),
        CASES.len(),
        "the oracle printed {} lines for {} cases\n{lines:#?}",
        lines.len(),
        CASES.len()
    );

    for (case, line) in CASES.iter().zip(&lines) {
        let line = line.trim();
        match case.want {
            Want::BadArg => assert_eq!(
                line, "badarg",
                "{} should be badarg in the oracle",
                describe(case)
            ),
            Want::Text(want) => {
                let inner = line
                    .strip_prefix("{ok,[")
                    .and_then(|rest| rest.strip_suffix("]}"))
                    .unwrap_or_else(|| {
                        panic!("unparsable oracle line for {}: {line:?}", describe(case))
                    });
                let bytes: Vec<u8> = if inner.is_empty() {
                    Vec::new()
                } else {
                    inner
                        .split(',')
                        .map(|field| {
                            field.parse::<u8>().unwrap_or_else(|e| {
                                panic!(
                                    "oracle character code {field:?} for {} is not a byte: {e}",
                                    describe(case)
                                )
                            })
                        })
                        .collect()
                };
                let text = String::from_utf8(bytes).expect("the oracle printed UTF-8");
                assert_eq!(text, want, "the pinned golden for {} is stale", describe(case));
            }
        }
    }
}
