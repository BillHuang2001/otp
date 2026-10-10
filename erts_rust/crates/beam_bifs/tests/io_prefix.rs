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

//! Oracle cross-checks for the `~x`/`~X` **prefix** corner of the shared
//! `io`/`io_lib` format engine (public API only, driven through [`BifRegistry`]).
//!
//! `io_lib_format:control_small($x,…)` guards its non-atom `Prefix` with
//! `true = io_lib:deep_char_list(Prefix)` and then passes `Prefix` through
//! **unmapped** (`prefixed_integer` builds `term([$-,Prefix|S], …)` /
//! `term([Prefix|S], …)`), so `io_lib:format("~.16x",[255,[256]])` ⇒
//! `[256,102,102]` — a prefix element is **not** restricted to a byte.
//! `lib/stdlib/src/io_lib.erl`'s `deep_char_list/1` accepts an integer `0 =< C <
//! 16#D800`, `16#DFFF < C < 16#FFFE`, `16#FFFF < C =< 16#10FFFF` (recursing into
//! nested lists) and `false` otherwise; `chars_length/1` counts a raw integer as
//! ONE character, so the field width / truncation counts each prefix element once.
//!
//! Both sides are compared **element by element**: the Rust side renders
//! `io_lib:format(F, A)` and prints the flattened element codes, the oracle side
//! prints `~w` of its own `io_lib:format(F, A)` (a flat list of integers, since
//! `~x`/`~X` is a "small" control spliced flat). [`the_pinned_goldens_match_the_registry`]
//! pins the expectations against this crate; [`the_pinned_goldens_match_the_live_oracle`]
//! re-derives all of them live from the system `erl` and SKIPS LOUDLY when no
//! oracle is available (the crate's established pattern), so the two together
//! assert *registry == oracle*. [`the_write_path_matches_the_live_oracle`]
//! re-derives the `io:format` **bytes** the same way.
//!
//! The `~c`/`~tc` code-point corner lives in the sibling `tests/io_charcode.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{is_list, is_small, signed_value, AtomTable, Eterm};

/// One Erlang argument, built on the Rust side as a term and on the oracle side as
/// Erlang source text.
#[derive(Clone, Copy)]
enum Arg {
    /// A small integer.
    Small(i64),
    /// An atom (its name is the `~x`/`~X` prefix).
    Atom(&'static str),
    /// A character list (an Erlang string) of ASCII characters.
    Str(&'static str),
    /// A proper list of small integers (a character list that may hold code points
    /// above 255).
    Ints(&'static [i64]),
    /// A character list holding one nested list: `[[inner], n]`.
    NestedList(&'static [i64], i64),
    /// An improper list `[head | tail]`.
    Improper(i64, i64),
    /// A binary of the given bytes.
    Binary(&'static [u8]),
}

/// The oracle's rendering of a case.
#[derive(Clone, Copy)]
enum Want {
    /// The flattened element codes of `io_lib:format(F, A)` — each element's
    /// decimal, comma-joined.
    Codes(&'static str),
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

/// Every case, each expectation captured from erts-17.1 with
/// `erl -noshell -eval 'io:format("~w~n",[io_lib:format(F,A)]).'`.
const CASES: &[Case] = &[
    // ---- non-byte prefix elements ---------------------------------------------
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::Ints(&[256])],
        want: Want::Codes("256,102,102"),
    },
    Case {
        format: "~.16X",
        args: &[Arg::Small(255), Arg::Ints(&[256])],
        want: Want::Codes("256,70,70"),
    },
    Case {
        format: "~.16X",
        args: &[Arg::Small(-255), Arg::Ints(&[256])],
        want: Want::Codes("45,256,70,70"),
    },
    Case {
        format: "~.16x",
        args: &[Arg::Small(-255), Arg::Ints(&[256])],
        want: Want::Codes("45,256,102,102"),
    },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[])], want: Want::Codes("102,102") },
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::Ints(&[300])],
        want: Want::Codes("300,102,102"),
    },
    // The largest accepted code point (`16#10FFFF`) and a value past it (rejected).
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::Ints(&[1114111])],
        want: Want::Codes("1114111,102,102"),
    },
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::Ints(&[65536])],
        want: Want::Codes("65536,102,102"),
    },
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::Ints(&[256, 0, 255])],
        want: Want::Codes("256,0,255,102,102"),
    },
    // A nested list is walked recursively (`deep_char_list/1`).
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::NestedList(&[65], 66)],
        want: Want::Codes("65,66,102,102"),
    },
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::Ints(&[65, 66])],
        want: Want::Codes("65,66,102,102"),
    },
    // The bare `~x`/`~X` (base 10 by default) keeps the raw element too.
    Case {
        format: "~x",
        args: &[Arg::Small(255), Arg::Ints(&[256])],
        want: Want::Codes("256,50,53,53"),
    },
    Case {
        format: "~X",
        args: &[Arg::Small(255), Arg::Ints(&[256])],
        want: Want::Codes("256,50,53,53"),
    },
    // The field width counts each prefix element as one character (`term/6` with
    // `latin1` uses `io_lib:chars_length/1`).
    Case {
        format: "~10.16x",
        args: &[Arg::Small(255), Arg::Ints(&[256])],
        want: Want::Codes("32,32,32,32,32,32,32,256,102,102"),
    },
    Case {
        format: "~5.16x",
        args: &[Arg::Small(255), Arg::Ints(&[256])],
        want: Want::Codes("32,32,256,102,102"),
    },
    Case {
        format: "~-10.16x",
        args: &[Arg::Small(255), Arg::Ints(&[256])],
        want: Want::Codes("256,102,102,32,32,32,32,32,32,32"),
    },
    // The signed `~#`/`~+` forms keep the sign ahead of their own `Base#` prefix.
    Case { format: "~.16#", args: &[Arg::Small(-255)], want: Want::Codes("45,49,54,35,70,70") },
    Case { format: "~.16+", args: &[Arg::Small(-255)], want: Want::Codes("45,49,54,35,102,102") },
    // A prefix element outside the oracle's character range, an improper list and
    // a non-list/non-atom prefix are all `badarg` (`io_lib:deep_char_list/1`).
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[-1])], want: Want::BadArg },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[65534])], want: Want::BadArg },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[65535])], want: Want::BadArg },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[55296])], want: Want::BadArg },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[1114112])], want: Want::BadArg },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Improper(1, 2)], want: Want::BadArg },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Binary(b"1")], want: Want::BadArg },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Small(16)], want: Want::BadArg },
    // A non-integer first argument has no `control_small` clause at all (`badarg`).
    Case { format: "~.16x", args: &[Arg::Str("z"), Arg::Ints(&[256])], want: Want::BadArg },
    // An atom prefix is its name, emitted as characters; a character list is its own
    // characters (both are `deep_char_list`-valid).
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Atom("ab")], want: Want::Codes("97,98,102,102") },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Atom("abc")], want: Want::Codes("97,98,99,102,102") },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Str("ab")], want: Want::Codes("97,98,102,102") },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Str("")], want: Want::Codes("102,102") },
];

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "ab", "abc"]);
    atoms
}

/// Materializes an [`Arg`] on `ctx`'s heap.
fn build_arg(ctx: &mut BifContext<'_>, arg: &Arg) -> Eterm {
    match arg {
        Arg::Small(value) => ctx.int(*value).expect("a small int fits"),
        Arg::Atom(name) => ctx.atom(name).expect("a seeded atom"),
        Arg::Str(text) => {
            let elems: Vec<Eterm> = text
                .bytes()
                .map(|b| ctx.int(i64::from(b)).expect("a character code fits"))
                .collect();
            ctx.list(&elems).expect("a short list fits")
        }
        Arg::Ints(values) => {
            let elems: Vec<Eterm> = values
                .iter()
                .map(|&v| ctx.int(v).expect("a code point fits"))
                .collect();
            ctx.list(&elems).expect("a short list fits")
        }
        Arg::NestedList(inner, tail) => {
            let inner_elems: Vec<Eterm> = inner
                .iter()
                .map(|&v| ctx.int(v).expect("a code point fits"))
                .collect();
            let inner = ctx.list(&inner_elems).expect("a short list fits");
            let tail = ctx.int(*tail).expect("a code point fits");
            ctx.list(&[inner, tail]).expect("a short list fits")
        }
        Arg::Improper(head, tail) => {
            let head = ctx.int(*head).expect("a small int fits");
            let tail = ctx.int(*tail).expect("a small int fits");
            ctx.cons(head, tail).expect("a cons fits")
        }
        Arg::Binary(bytes) => {
            beam_codec::term::make_binary(ctx.heap(), bytes).expect("a short binary fits")
        }
    }
}

/// Flattens an `io_lib:format` result to its element codes: each element's decimal
/// (`~w`-style), comma-joined. `deep_char_list`-valid prefix elements are small
/// integers, so a small-integer leaf is all this corpus produces.
fn element_codes(term: Eterm) -> String {
    let mut out: Vec<String> = Vec::new();
    collect_codes(term, &mut out);
    out.join(",")
}

fn collect_codes(term: Eterm, out: &mut Vec<String>) {
    if is_small(term) {
        out.push(signed_value(term).to_string());
        return;
    }
    let mut cursor = term;
    while is_list(cursor) {
        collect_codes(beam_codec::term::cons_head(cursor).expect("a cons head"), out);
        cursor = beam_codec::term::cons_tail(cursor).expect("a cons tail");
    }
    assert_eq!(cursor, beam_term::NIL, "io_lib:format returned an improper list");
}

/// Renders one case through `io_lib:format/2`, returning its element codes or the
/// typed `badarg`.
fn render(ctx: &mut BifContext<'_>, reg: &BifRegistry, case: &Case) -> Result<String> {
    let format = ctx
        .list(
            &case
                .format
                .bytes()
                .map(|b| ctx.int(i64::from(b)).expect("a byte fits"))
                .collect::<Vec<Eterm>>(),
        )
        .expect("a short list fits");
    let mut elems = Vec::with_capacity(case.args.len());
    for arg in case.args {
        elems.push(build_arg(ctx, arg));
    }
    let args = ctx.list(&elems).expect("a short argument list fits");
    let out = reg.call("io_lib", "format", 2, ctx, &[format, args])?;
    Ok(element_codes(out))
}

/// Describes a case for the assertion messages.
fn describe(case: &Case) -> String {
    let args: Vec<String> = case.args.iter().map(source_of).collect();
    format!("io_lib:format({:?}, [{}])", case.format, args.join(", "))
}

/// The Erlang source of one argument.
fn source_of(arg: &Arg) -> String {
    match arg {
        Arg::Small(value) => value.to_string(),
        Arg::Atom(name) => (*name).to_owned(),
        // An Erlang string literal — every case is ASCII with no escapes.
        Arg::Str(text) => format!("{text:?}"),
        Arg::Ints(values) => format!(
            "[{}]",
            values.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
        ),
        Arg::NestedList(inner, tail) => format!(
            "[[{}],{tail}]",
            inner.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
        ),
        Arg::Improper(head, tail) => format!("[{head}|{tail}]"),
        Arg::Binary(bytes) => format!(
            "<<{}>>",
            bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
        ),
    }
}

/// The Erlang source of an argument list.
fn args_source(args: &[Arg]) -> String {
    format!(
        "[{}]",
        args.iter().map(source_of).collect::<Vec<_>>().join(",")
    )
}

/// The pinned goldens are what this crate's registry renders.
#[test]
#[ignore = "fix (2) not implemented yet: `~x`/`~X` reject a non-byte prefix element \
            with `badarg`; the oracle emits the integer elements of a deep character \
            list verbatim. Un-ignored once `src/io.rs`'s `prefix_arg` walks the prefix."]
fn the_pinned_goldens_match_the_registry() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    for case in CASES {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let got = render(&mut ctx, &reg, case);
        match (got, case.want) {
            (Ok(codes), Want::Codes(want)) => {
                assert_eq!(codes, want, "{} does not match the pinned golden", describe(case));
            }
            (Err(BeamError::BadArg), Want::BadArg) => {}
            (Ok(codes), Want::BadArg) => {
                panic!("{} should be badarg, rendered {codes:?}", describe(case));
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
            "[beam_bifs io_prefix] $ERTS_ORACLE='{}' is not an existing executable \
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

/// The element codes of the oracle's `~w` rendering of a flat integer list
/// (`[256,102,102]` ⇒ `256,102,102`).
fn parse_codes_line(line: &str) -> String {
    let inner = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("unparsable oracle line: {line:?}"));
    if inner.is_empty() {
        return String::new();
    }
    inner
        .split(',')
        .map(|field| {
            field
                .trim()
                .parse::<i128>()
                .unwrap_or_else(|e| panic!("the oracle element {field:?} is not an integer: {e}"))
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Every pinned golden is the live oracle's own output. Skips loudly when no
/// oracle is available.
#[test]
#[ignore = "fix (2) not implemented yet: `~x`/`~X` reject a non-byte prefix element \
            with `badarg`; the oracle emits the integer elements of a deep character \
            list verbatim. Un-ignored once `src/io.rs`'s `prefix_arg` walks the prefix."]
fn the_pinned_goldens_match_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=========== beam_bifs io_prefix: live oracle SKIPPED ===========");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("==============================================================");
        eprintln!();
        return;
    };

    // One session for the whole table: render each case with `~w` (which prints a
    // flat integer list, never re-flowing), or the atom `badarg`.
    let cases: Vec<String> = CASES
        .iter()
        .map(|case| format!("{{{:?},{}}}", case.format, args_source(case.args)))
        .collect();
    let eval = format!(
        "lists:foreach(fun({{F,A}}) -> R = try io_lib:format(F,A) \
         of V -> V catch _:_ -> badarg end, io:format(\"~w~n\",[R]) end, [{}]).",
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
            Want::Codes(want) => {
                let codes = parse_codes_line(line);
                assert_eq!(
                    codes,
                    want,
                    "the pinned golden for {} is stale (oracle line {line:?})",
                    describe(case)
                );
            }
        }
    }
}

/// One `io:format(F, A)` case: the format and its arguments.
struct WriteCase {
    format: &'static str,
    args: &'static [Arg],
}

impl WriteCase {
    /// The format string as an Erlang literal.
    fn args_format(&self) -> String {
        format!("{:?}", self.format)
    }

    /// The argument list as Erlang source.
    fn args_source(&self) -> String {
        args_source(self.args)
    }
}

/// The `io:format` **write** path: the bytes the crate hands its sink are the
/// oracle's own stdout bytes — a `deep_char_list`-valid prefix element is its
/// UTF-8 encoding.
#[test]
#[ignore = "fix (2) not implemented yet: `~x`/`~X` reject a non-byte prefix element \
            with `badarg`; the oracle emits the integer elements of a deep character \
            list verbatim. Un-ignored once `src/io.rs`'s `prefix_arg` walks the prefix."]
fn the_write_path_matches_the_live_oracle() {
    const WRITE_CASES: &[WriteCase] = &[
        WriteCase { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[256])] },
        WriteCase { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[0x10FFFF])] },
        WriteCase { format: "~.16x", args: &[Arg::Small(255), Arg::Str("ab")] },
        WriteCase { format: "~.16x", args: &[Arg::Small(255), Arg::Atom("ab")] },
    ];

    // The refusal cases: the oracle's device rejects the code point, so
    // `io:format` raises `badarg` and writes nothing.
    const REFUSED: &[WriteCase] = &[
        WriteCase { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[0xFFFE])] },
    ];

    // The crate's own bytes.
    let registry = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let mut ours: Vec<Vec<u8>> = Vec::new();
    for case in WRITE_CASES {
        let mut sink = BytesSink::new();
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            let format = ctx
                .list(
                    &case
                        .format
                        .bytes()
                        .map(|b| ctx.int(i64::from(b)).expect("a byte fits"))
                        .collect::<Vec<Eterm>>(),
                )
                .expect("a short list fits");
            let elems: Vec<Eterm> = case.args.iter().map(|a| build_arg(&mut ctx, a)).collect();
            let args = ctx.list(&elems).expect("a short argument list fits");
            registry
                .call("io", "format", 2, &mut ctx, &[format, args])
                .unwrap_or_else(|e| panic!("io:format({:?}, …) failed: {e:?}", case.format));
        }
        ours.push(sink.contents().to_vec());
    }
    for case in REFUSED {
        let mut sink = BytesSink::new();
        let result;
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            let format = ctx
                .list(
                    &case
                        .format
                        .bytes()
                        .map(|b| ctx.int(i64::from(b)).expect("a byte fits"))
                        .collect::<Vec<Eterm>>(),
                )
                .expect("a short list fits");
            let elems: Vec<Eterm> = case.args.iter().map(|a| build_arg(&mut ctx, a)).collect();
            let args = ctx.list(&elems).expect("a short argument list fits");
            result = registry.call("io", "format", 2, &mut ctx, &[format, args]);
        }
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "io:format({:?}, …) should be badarg",
            case.format
        );
        assert!(sink.contents().is_empty(), "nothing is written on badarg");
    }

    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=========== beam_bifs io_prefix: live oracle SKIPPED ===========");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("==============================================================");
        eprintln!();
        return;
    };

    // The oracle's own stdout bytes, one process per case (the stream must not be
    // mixed with a `~w` frame). `-noshell` writes the device's bytes verbatim.
    for (case, want) in WRITE_CASES.iter().zip(&ours) {
        let eval = format!("io:format({},{}).", case.args_format(), case.args_source());
        let output = Command::new(&oracle)
            .arg("-noshell")
            .arg("-eval")
            .arg(&eval)
            .arg("-s")
            .arg("init")
            .arg("stop")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
        assert!(
            output.status.success(),
            "the oracle exited with {:?} for {eval}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            &output.stdout, want,
            "io:format({}, {}) wrote {:?} on the oracle but {:?} here",
            case.args_format(),
            case.args_source(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(want)
        );
    }

    // And the refusal cases really do raise on the oracle.
    for case in REFUSED {
        let eval = format!(
            "try io:format({},{}) of _ -> io:format(\"no_error\") \
             catch _:_ -> io:format(\"badarg\") end.",
            case.args_format(),
            case.args_source()
        );
        let output = Command::new(&oracle)
            .arg("-noshell")
            .arg("-eval")
            .arg(&eval)
            .arg("-s")
            .arg("init")
            .arg("stop")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
        assert_eq!(
            output.stdout, b"badarg",
            "io:format({}, {}) should raise on the oracle, stdout was {:?}",
            case.args_format(),
            case.args_source(),
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
