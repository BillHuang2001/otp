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

//! Oracle cross-checks for the two `Chunk`-shaped corners of the shared
//! `io`/`io_lib` format engine (public API only, driven through [`BifRegistry`]):
//!
//! 1. **`~c`/`~tc` with a code point outside the `char` range.** The oracle's
//!    `io_lib_format:control_small($c,…)` has two clauses: `~tc` renders the
//!    integer **verbatim** (`char(A, …)` is `[A]`) and `~c` renders only its low
//!    byte (`char(A band 255, …)`). So `io_lib:format("~tc",[1114112])` ⇒
//!    `[1114112]`, `~tc` of the surrogate `55296` ⇒ `[55296]`, `~tc` of `-1` ⇒
//!    `[-1]`, `~tc` of a bignum ⇒ that bignum, while `~c` of `257`/`-1`/
//!    `1 bsl 100` ⇒ `[1]`/`[255]`/`[0]`. The rendered code point travels as a
//!    [`beam_bifs`]-internal raw-integer chunk; `io:format` then writes a valid
//!    scalar as its UTF-8 encoding and raises `badarg` for one the oracle's
//!    unicode device refuses (`io:format("~tc",[1114112])` ⇒ `error:badarg`).
//! 2. **Base directives with non-byte prefix elements.** `~x`/`~X`'s `Prefix` is
//!    an atom (its name) or a **deep** character list (`io_lib:deep_char_list/1`)
//!    whose elements are emitted **verbatim**, so
//!    `io_lib:format("~.16x",[255,[256]])` ⇒ `[256,102,102]` — an element is not
//!    restricted to a byte.
//!
//! Both sides are compared **element by element**: the Rust side renders
//! `io_lib:format(F, A)` and prints the flattened element codes, the oracle side
//! prints `~w` of its own `io_lib:format(F, A)` (a flat list of integers, since
//! every one of these controls is spliced flat). [`the_pinned_goldens_match_the_registry`]
//! pins the expectations against this crate; [`the_pinned_goldens_match_the_live_oracle`]
//! re-derives all of them live from the system `erl` and SKIPS LOUDLY when no
//! oracle is available (the crate's established pattern), so the two together
//! assert *registry == oracle*. [`the_write_path_matches_the_live_oracle`]
//! re-derives the `io:format` **bytes** the same way.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{is_big, is_small, signed_value, AtomTable, Eterm};

/// The limbs of `1 bsl 100` (`2^100 = 2^64 * 2^36`), least significant first.
const SHIFTED_100: &[u64] = &[0, 1 << 36];

/// The decimal literal of [`SHIFTED_100`].
const SHIFTED_100_SRC: &str = "1267650600228229401496703205376";

/// The limbs of `(1 bsl 64) + 255`: a bignum whose low byte is `255`.
const BIGNUM_LOW_255: &[u64] = &[255, 1];

/// The decimal literal of [`BIGNUM_LOW_255`].
const BIGNUM_LOW_255_SRC: &str = "18446744073709551871";

/// One Erlang argument, built on the Rust side as a term and on the oracle side as
/// Erlang source text.
#[derive(Clone, Copy)]
enum Arg {
    /// A small integer.
    Small(i64),
    /// An arbitrary-precision integer: `(negative, limbs, decimal source)`.
    Big(bool, &'static [u64], &'static str),
    /// A character list (an Erlang string).
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
    // ---- fix (1): `~tc` carries the code point verbatim ------------------------
    Case { format: "~tc", args: &[Arg::Small(1114112)], want: Want::Codes("1114112") },
    Case { format: "~tc", args: &[Arg::Small(0x110000)], want: Want::Codes("1114112") },
    Case { format: "~tc", args: &[Arg::Small(55296)], want: Want::Codes("55296") },
    Case { format: "~tc", args: &[Arg::Small(-1)], want: Want::Codes("-1") },
    Case { format: "~tc", args: &[Arg::Small(-1114112)], want: Want::Codes("-1114112") },
    Case { format: "~tc", args: &[Arg::Small(0)], want: Want::Codes("0") },
    Case { format: "~tc", args: &[Arg::Small(1)], want: Want::Codes("1") },
    Case { format: "~tc", args: &[Arg::Small(0x10FFFF)], want: Want::Codes("1114111") },
    Case { format: "~tc", args: &[Arg::Small(0x100)], want: Want::Codes("256") },
    Case {
        format: "~tc",
        args: &[Arg::Big(false, SHIFTED_100, SHIFTED_100_SRC)],
        want: Want::Codes(SHIFTED_100_SRC),
    },
    Case {
        format: "~tc",
        args: &[Arg::Big(true, SHIFTED_100, SHIFTED_100_SRC)],
        want: Want::Codes("-1267650600228229401496703205376"),
    },
    // `~tc` duplicates a raw code point through `chars/2` (`~3tc`, `~3.2tc`) and
    // rejects a non-integer.
    Case { format: "~3tc", args: &[Arg::Small(1114112)], want: Want::Codes("1114112,1114112,1114112") },
    Case { format: "~3.2tc", args: &[Arg::Small(1114112)], want: Want::Codes("32,1114112,1114112") },
    Case { format: "~2.1tc", args: &[Arg::Small(1114112)], want: Want::Codes("32,1114112") },
    Case { format: "~5tc", args: &[Arg::Small(-1)], want: Want::Codes("-1,-1,-1,-1,-1") },
    Case { format: "~tc", args: &[Arg::Str("a")], want: Want::BadArg },
    // ---- fix (1): `~c` keeps only the low byte --------------------------------
    Case { format: "~c", args: &[Arg::Small(256)], want: Want::Codes("0") },
    Case { format: "~c", args: &[Arg::Small(257)], want: Want::Codes("1") },
    Case { format: "~c", args: &[Arg::Small(-1)], want: Want::Codes("255") },
    Case { format: "~c", args: &[Arg::Small(255)], want: Want::Codes("255") },
    Case { format: "~c", args: &[Arg::Small(1114112)], want: Want::Codes("0") },
    Case { format: "~c", args: &[Arg::Small(55296)], want: Want::Codes("0") },
    Case {
        format: "~c",
        args: &[Arg::Big(false, SHIFTED_100, SHIFTED_100_SRC)],
        want: Want::Codes("0"),
    },
    Case {
        format: "~c",
        args: &[Arg::Big(true, SHIFTED_100, SHIFTED_100_SRC)],
        want: Want::Codes("0"),
    },
    Case {
        format: "~c",
        args: &[Arg::Big(false, BIGNUM_LOW_255, BIGNUM_LOW_255_SRC)],
        want: Want::Codes("255"),
    },
    Case {
        format: "~c",
        args: &[Arg::Big(true, BIGNUM_LOW_255, BIGNUM_LOW_255_SRC)],
        want: Want::Codes("1"),
    },
    Case { format: "~c", args: &[Arg::Str("a")], want: Want::BadArg },
    // ---- fix (2): non-byte prefix elements ------------------------------------
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
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[])], want: Want::Codes("102,102") },
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::Ints(&[300])],
        want: Want::Codes("300,102,102"),
    },
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
        args: &[Arg::Small(255), Arg::NestedList(&[65], 66)],
        want: Want::Codes("65,66,102,102"),
    },
    Case {
        format: "~.16x",
        args: &[Arg::Small(255), Arg::Ints(&[65, 66])],
        want: Want::Codes("65,66,102,102"),
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
    // An atom prefix is its name, emitted as characters.
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Str("ab")], want: Want::Codes("97,98,102,102") },
    Case { format: "~.16x", args: &[Arg::Small(255), Arg::Str("")], want: Want::Codes("102,102") },
];

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "abc"]);
    atoms
}

/// Materializes an [`Arg`] on `ctx`'s heap.
fn build_arg(ctx: &mut BifContext<'_>, arg: &Arg) -> Eterm {
    match arg {
        Arg::Small(value) => ctx.int(*value).expect("a small int fits"),
        Arg::Big(negative, limbs, _) => {
            beam_codec::term::make_big(ctx.heap(), *negative, limbs).expect("a test bignum")
        }
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
        Arg::Binary(bytes) => ctx
            .make_binary(bytes)
            .expect("a short binary fits"),
    }
}

/// Flattens an `io_lib:format` result to its element codes: each element's decimal
/// (`~w`-style), comma-joined. A sub-list is flattened (every control in the table
/// is spliced flat by `build_small`).
fn element_codes(term: Eterm) -> String {
    let mut out: Vec<String> = Vec::new();
    collect_codes(term, &mut out);
    out.join(",")
}

fn collect_codes(term: Eterm, out: &mut Vec<String>) {
    if !is_small(term) && !is_big(term) {
        let mut cursor = term;
        while is_list(cursor) {
            collect_codes(beam_codec::term::cons_head(cursor).expect("a cons head"), out);
            cursor = beam_codec::term::cons_tail(cursor).expect("a cons tail");
        }
        assert_eq!(cursor, beam_term::NIL, "io_lib:format returned an improper list");
        return;
    }
    out.push(if is_small(term) {
        signed_value(term).to_string()
    } else {
        beam_codec::term::big_parts(term)
            .map(|(negative, limbs)| {
                // `~w` prints the bignum's decimal; render it through the crate's
                // own integer renderer by round-tripping the term through `~b`.
                format!(
                    "{}{}",
                    if negative { "-" } else { "" },
                    decimal_of(limbs)
                )
            })
            .expect("a bignum splits")
    });
}

/// The decimal rendering of a little-endian `u64` limb vector.
fn decimal_of(limbs: &[u64]) -> String {
    let mut digits: Vec<u32> = vec![0];
    for &limb in limbs.iter().rev() {
        // digits = digits * 2^64 + limb (base 10^9).
        let mut carry = limb as u128;
        for digit in digits.iter_mut() {
            let value = u128::from(*digit) << 64 | carry;
            *digit = (value % 1_000_000_000) as u32;
            carry = value / 1_000_000_000;
        }
        while carry > 0 {
            digits.push((carry % 1_000_000_000) as u32);
            carry /= 1_000_000_000;
        }
    }
    let mut out = String::new();
    for (index, digit) in digits.iter().enumerate().rev() {
        if index == digits.len() - 1 {
            out.push_str(&digit.to_string());
        } else {
            out.push_str(&format!("{digit:09}"));
        }
    }
    if out.is_empty() {
        out.push('0');
    }
    out
}

use beam_term::is_list;

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
    let args: Vec<String> = case
        .args
        .iter()
        .map(|arg| match arg {
            Arg::Small(value) => value.to_string(),
            Arg::Big(negative, _, src) => format!("{}{src}", if *negative { "-" } else { "" }),
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
            "[beam_bifs io_charcode] $ERTS_ORACLE='{}' is not an existing executable \
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
            Arg::Big(negative, _, src) => format!("{}{src}", if *negative { "-" } else { "" }),
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

/// The element codes of the oracle's `~w` rendering of a flat integer list
/// (`[1114112,102,102]` ⇒ `1114112,102,102`).
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
fn the_pinned_goldens_match_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=========== beam_bifs io_charcode: live oracle SKIPPED ===========");
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

/// The `io:format` **write** path: the bytes the crate hands its sink are the
/// oracle's own stdout bytes — the UTF-8 encoding of a valid code point, or
/// nothing at all (the call raises `badarg`).
#[test]
fn the_write_path_matches_the_live_oracle() {
    /// One `io:format(F, A)` case: the format, its arguments as Erlang source, and
    /// the argument list built on the Rust side.
    struct WriteCase {
        format: &'static str,
        args: &'static [Arg],
    }

    const WRITE_CASES: &[WriteCase] = &[
        WriteCase { format: "~tc", args: &[Arg::Small(0x10FFFF)] },
        WriteCase { format: "~tc", args: &[Arg::Small(0x100)] },
        WriteCase { format: "~tc", args: &[Arg::Small(0)] },
        WriteCase { format: "~tc", args: &[Arg::Small(1)] },
        WriteCase { format: "~c", args: &[Arg::Small(255)] },
        WriteCase { format: "~c", args: &[Arg::Small(256)] },
        WriteCase { format: "~c", args: &[Arg::Small(-1)] },
        WriteCase { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[256])] },
        WriteCase { format: "~.16x", args: &[Arg::Small(255), Arg::Ints(&[0x10FFFF])] },
    ];

    // The refusal cases: the oracle's device rejects the code point, so
    // `io:format` raises `badarg` and writes nothing.
    const REFUSED: &[WriteCase] = &[
        WriteCase { format: "~tc", args: &[Arg::Small(1114112)] },
        WriteCase { format: "~tc", args: &[Arg::Small(55296)] },
        WriteCase { format: "~tc", args: &[Arg::Small(-1)] },
        WriteCase { format: "~tc", args: &[Arg::Big(false, SHIFTED_100, SHIFTED_100_SRC)] },
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
        eprintln!("=========== beam_bifs io_charcode: live oracle SKIPPED ===========");
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
        let eval = format!("io:format({:?},{}).", case.args_format(), case.args_source());
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
