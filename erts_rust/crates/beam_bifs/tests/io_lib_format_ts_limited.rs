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
//! Integration tests for the **unicode** `~ts` directive of `io_lib:format/3`
//! under an active `{chars_limit, N}` option — the oracle's
//! `io_lib_format:control_limited($s, [L0], F, Adj, P, Pad, unicode, …)`.
//!
//! ```text
//! L = cdata_to_chars(L0, F, CL),
//! uniconv(string(L, limit_field(F, CL), Adj, P, Pad, unicode)).
//! ```
//!
//! `uniconv/1` is the identity, so the arm is `cdata_to_chars/3` + `string/6` —
//! *not* the latin1 `~s` arm: the limit walks `string:next_grapheme/1` (grapheme
//! clusters, each an integer or a **list** of code points), the elision probe counts
//! with `string:length/1`, `string/6` measures with `io_lib:chars_length/1`
//! (`iolist_size/1` with a `string:length/1` fallback — the two disagree), the
//! truncation is `string:slice/3` (whose cluster splice is a flat `++`), and the
//! element list can carry a real **binary** (`[[<<200,201>>],97]` at `CL >= 4`
//! renders `<<200,201>>` verbatim).
//!
//! # The golden matrix
//!
//! `tests/data/io_lib_format_ts_limited.goldens` is the literal output of the
//! system oracle (OTP 29.1.1 / erts-17.1) for
//!
//! ```text
//! 21 subjects × 15 formats × chars_limits [-1,0..11,20,100]
//!   => try io_lib:format(Fmt, [Subject], [{chars_limit, CL}]) catch error:badarg -> badarg end
//! ```
//!
//! captured as `~w` so the *structure* — not the flattened text — is visible: a row
//! is `Fmt|Subject|CL => Value`, and a raising case is `{error,badarg}`. Every
//! result is compared with `beam_term::compare::eq`, so a differing nested sub-list
//! is a failure even when the flattened text agrees.
//!
//! Rows are the oracle's own, not hand-transcribed: the file is
//! `sed -n '1,4725p' rows.txt` of the probe escript, and
//! [`the_golden_matrix_matches_the_committed_oracle_rows`] re-checks the row count
//! and the ordering, so a truncated or reordered file fails the build.
//!
//! # The live-oracle cross-check
//!
//! [`unlimited_and_limited_ts_cross_check_the_live_oracle`] re-derives **every**
//! row against the installed `erl` and requires the `erlang:term_to_binary/1` image
//! of every result to agree byte-for-byte. A missing oracle is a **failure**, never
//! a silent pass: this arm silently produced `unsupported_bif` before, so skipping
//! the check when no oracle is installed would re-create that blindness.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_codec::term` for the
//! binary/float constructors, `beam_term` for the term readers and comparison).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{bitstring_bytes, cons_head, cons_tail, make_binary};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{is_nil, is_small, signed_value, AtomTable, Eterm};

/// The committed oracle matrix: one `Fmt|Subject|CL => Value` row per case.
const GOLDEN: &str = include_str!("data/io_lib_format_ts_limited.goldens");

/// The 21 matrix subjects, in the probe's order.
const SUBJECTS: [&str; 21] = [
    "U",
    "A",
    "Z",
    "KOU",
    "KOU2",
    "CRLF",
    "SUR",
    "OOR",
    "OOR1",
    "NESTBAD",
    "NESTBAD2",
    "FLATBAD",
    "FLATBAD2",
    "EMPTY",
    "EMPTYBIN",
    "IMP",
    "INT",
    "NEG",
    "FLOAT",
    "ATOM",
    "BAD",
];

/// The 15 matrix formats, in the probe's order.
const FORMATS: [&str; 15] = [
    "~ts",
    "a~tsb",
    "~10ts",
    "~-10ts",
    "~10.3ts",
    "~10.4ts",
    "~0ts",
    "~4ts",
    "~1ts",
    "~2ts",
    "~.3ts",
    "~8ts",
    "~6ts",
    "~-ts",
    "~5ts",
];

/// The 15 matrix `{chars_limit, N}` values, in the probe's order.
const LIMITS: [i64; 15] = [-1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 20, 100];

/// The number of matrix rows (a sanity floor, not a substitute for the file).
const MATRIX_ROWS: usize = 21 * 15 * 15;

/// The atoms every fixture seeds: the option tag, the atom the oracle side renders
/// for a raising case (`try … catch error:badarg -> badarg end`), the `ATOM`
/// subject, the `error` tag `string:next_grapheme/1` builds an `{error, Err}` with,
/// and the atoms `io_lib` itself may intern.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "chars_limit",
        "badarg",
        "abc",
        "error",
        "true",
        "false",
        "ok",
    ]);
    atoms
}

/// A small integer on `ctx`'s heap.
fn int(ctx: &mut BifContext<'_>, value: i64) -> Eterm {
    ctx.int(value).expect("a small int fits")
}

/// A byte-aligned binary `<<bytes>>` on `ctx`'s heap.
fn binary(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    make_binary(ctx.heap(), bytes).expect("the binary fits")
}

/// A proper list of the (already built) `items` on `ctx`'s heap.
fn list(ctx: &mut BifContext<'_>, items: &[Eterm]) -> Eterm {
    ctx.list(items).expect("a short list fits")
}

/// A seeded atom on `ctx`'s heap.
fn atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("the atom is seeded")
}

/// Builds subject `name` on `ctx` — the 21 matrix subjects, exactly as the probe
/// escript defines them.
fn subject(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    let codepoints = |ctx: &mut BifContext<'_>, values: &[i64]| {
        let items: Vec<Eterm> = values.iter().map(|&v| int(ctx, v)).collect();
        list(ctx, &items)
    };
    match name {
        "U" => codepoints(ctx, &[26085, 26412, 35486, 26085, 26412, 35486]),
        "A" => codepoints(ctx, &[97, 769, 98, 99]),
        "Z" => codepoints(ctx, &[128104, 8205, 128105, 8205, 128102]),
        "KOU" => binary(ctx, &[230, 151, 165]),
        "KOU2" => binary(ctx, &[230, 151, 165, 230, 151, 165]),
        "CRLF" => codepoints(ctx, &[13, 10, 97]),
        "SUR" => codepoints(ctx, &[55296, 97]),
        "OOR" => codepoints(ctx, &[1114112, 97]),
        "OOR1" => codepoints(ctx, &[1114112]),
        "NESTBAD" => {
            let inner = binary(ctx, &[200, 201]);
            let sub = list(ctx, &[inner]);
            let items = vec![sub, int(ctx, 97)];
            list(ctx, &items)
        }
        "NESTBAD2" => {
            let inner = binary(ctx, &[200, 201]);
            let sub = list(ctx, &[inner]);
            list(ctx, &[sub])
        }
        "FLATBAD" => {
            let items = vec![binary(ctx, &[200, 201]), int(ctx, 97)];
            list(ctx, &items)
        }
        "FLATBAD2" => {
            let b = binary(ctx, &[200, 201]);
            list(ctx, &[b])
        }
        "EMPTY" => list(ctx, &[]),
        "EMPTYBIN" => binary(ctx, &[]),
        "IMP" => {
            let head = int(ctx, 97);
            let tail = int(ctx, 98);
            ctx.cons(head, tail).expect("the cons fits")
        }
        "INT" => int(ctx, 97),
        "NEG" => codepoints(ctx, &[97, -1, 98]),
        "FLOAT" => {
            let f = ctx.make_float(1.5).expect("the float fits");
            list(ctx, &[f])
        }
        "ATOM" => atom(ctx, "abc"),
        "BAD" => binary(ctx, &[200, 201]),
        other => panic!("unknown matrix subject {other}"),
    }
}

/// Runs `io_lib:format(Fmt, [Subject], [{chars_limit, Limit}])` through the real
/// registry and returns its result term.
fn format_limited_ts(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    format: &str,
    limit: i64,
    subject: Eterm,
) -> Result<Eterm> {
    let bytes: Vec<Eterm> = format.bytes().map(|b| int(ctx, i64::from(b))).collect();
    let format_term = list(ctx, &bytes);
    let args_term = list(ctx, &[subject]);
    let tag = atom(ctx, "chars_limit");
    let limit_term = int(ctx, limit);
    let option = ctx.tuple(&[tag, limit_term]).expect("a short tuple fits");
    let options = list(ctx, &[option]);
    reg.call("io_lib", "format", 3, ctx, &[format_term, args_term, options])
}

// ---------------------------------------------------------------------------
// The committed-oracle value grammar
// ---------------------------------------------------------------------------

/// A golden value: the oracle's deep list, or the plain `badarg` a raising case
/// was folded into by `try … catch error:badarg -> badarg end`.
#[derive(Debug, PartialEq, Eq)]
enum Value {
    /// `{error,badarg}` — the row's call raised `error:badarg`.
    BadArg,
    /// The oracle's deep list.
    Deep(G),
}

/// The shape of a golden deep list: nested lists of small integers and
/// byte-aligned binaries (`<<200,201>>`).
#[derive(Debug, PartialEq, Eq)]
enum G {
    Int(i64),
    Bin(Vec<u8>),
    List(Vec<G>),
}

impl G {
    /// Materializes the shape on `ctx`'s heap.
    fn build(&self, ctx: &mut BifContext<'_>) -> Eterm {
        match self {
            G::Int(value) => int(ctx, *value),
            G::Bin(bytes) => binary(ctx, bytes),
            G::List(items) => {
                let elems: Vec<Eterm> = items.iter().map(|item| item.build(ctx)).collect();
                list(ctx, &elems)
            }
        }
    }
}

/// One parsed golden row.
struct Row {
    format: String,
    subject: String,
    limit: i64,
    value: Value,
}

impl Row {
    /// The `Fmt|Subject|CL` key this row was decoded from.
    fn key(&self) -> String {
        format!("{}|{}|{}", self.format, self.subject, self.limit)
    }
}

/// Parses the committed matrix, in file order.
fn golden_rows() -> Vec<Row> {
    GOLDEN
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_row)
        .collect()
}

/// Parses one `Fmt|Subject|CL => Value` line.
fn parse_row(line: &str) -> Row {
    let (key, value) = line
        .split_once("=>")
        .unwrap_or_else(|| panic!("malformed golden row {line:?}"));
    let mut fields = key.trim().split('|');
    let format = fields
        .next()
        .unwrap_or_else(|| panic!("missing format in {line:?}"))
        .to_string();
    let subject = fields
        .next()
        .unwrap_or_else(|| panic!("missing subject in {line:?}"))
        .to_string();
    let limit: i64 = fields
        .next()
        .unwrap_or_else(|| panic!("missing chars_limit in {line:?}"))
        .parse()
        .unwrap_or_else(|e| panic!("bad chars_limit in {line:?}: {e}"));
    assert!(
        fields.next().is_none(),
        "trailing `|` fields in {line:?}"
    );
    let value = value.trim();
    let value = if value == "{error,badarg}" {
        Value::BadArg
    } else {
        Value::Deep(parse_value(&mut value.chars().peekable()))
    };
    Row {
        format,
        subject,
        limit,
        value,
    }
}

/// A recursive-descent reader for the golden value grammar.
fn parse_value(chars: &mut std::iter::Peekable<std::str::Chars>) -> G {
    match chars.peek() {
        Some('[') => {
            chars.next();
            let mut items = Vec::new();
            if chars.peek() == Some(&']') {
                chars.next();
                return G::List(items);
            }
            loop {
                items.push(parse_value(chars));
                match chars.next() {
                    Some(',') => {}
                    Some(']') => break,
                    other => panic!("expected `,`/`]`, got {other:?}"),
                }
            }
            G::List(items)
        }
        Some('<') => {
            chars.next();
            assert_eq!(chars.next(), Some('<'), "a binary starts with `<<`");
            let mut bytes = Vec::new();
            if chars.peek() == Some(&'>') {
                chars.next();
                assert_eq!(chars.next(), Some('>'));
                return G::Bin(bytes);
            }
            loop {
                bytes.push(parse_number(chars) as u8);
                match chars.next() {
                    Some(',') => {}
                    Some('>') => {
                        assert_eq!(chars.next(), Some('>'));
                        break;
                    }
                    other => panic!("expected `,`/`>>`, got {other:?}"),
                }
            }
            G::Bin(bytes)
        }
        Some(_) => G::Int(parse_number(chars)),
        None => panic!("unexpected end of a golden value"),
    }
}

fn parse_number(chars: &mut std::iter::Peekable<std::str::Chars>) -> i64 {
    let mut text = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() || c == '-' {
            text.push(c);
            chars.next();
        } else {
            break;
        }
    }
    text.parse()
        .unwrap_or_else(|e| panic!("expected a number, got {text:?}: {e}"))
}

// ---------------------------------------------------------------------------
// The committed-golden matrix (no `erl` needed)
// ---------------------------------------------------------------------------

/// Asserts that every one of the 4725 rows renders the oracle's deep list (or
/// raises `badarg`).
///
/// This runs with **no** oracle — the goldens are the literal oracle output, so the
/// behaviour is pinned even in a sandbox without `erl`.
#[test]
fn the_golden_matrix_matches_the_committed_oracle_rows() {
    let reg = default_registry();
    let rows = golden_rows();
    assert_eq!(
        rows.len(),
        MATRIX_ROWS,
        "the committed `~ts`/`{{chars_limit, N}}` matrix must keep exactly \
         {MATRIX_ROWS} rows (21 subjects × 15 formats × 15 limits)"
    );
    // The file must be the probe's nested comprehension in order, so a reordered or
    // regressed file cannot silently re-associate a value with a case.
    let mut index = 0usize;
    for format in FORMATS {
        for subject in SUBJECTS {
            for limit in LIMITS {
                let row = &rows[index];
                assert_eq!(
                    row.key(),
                    format!("{format}|{subject}|{limit}"),
                    "golden row {index} is out of order"
                );
                index += 1;
            }
        }
    }
    for (index, row) in rows.iter().enumerate() {
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let arg = subject(&mut ctx, &row.subject);
        let got = format_limited_ts(&mut ctx, &reg, &row.format, row.limit, arg);
        match &row.value {
            Value::BadArg => match got {
                Err(BeamError::BadArg) => {}
                other => panic!(
                    "row {index} ({}): expected badarg, got {other:?}",
                    row.key()
                ),
            },
            Value::Deep(expected) => {
                let got = got.unwrap_or_else(|e| {
                    panic!(
                        "row {index} ({}): io_lib:format failed: {e:?}",
                        row.key()
                    )
                });
                let want = expected.build(&mut ctx);
                if !beam_term::compare::eq(got, want, ctx.atoms()) {
                    let got_hex = term_to_binary_hex(&mut ctx, &reg, got);
                    let want_hex = term_to_binary_hex(&mut ctx, &reg, want);
                    panic!(
                        "row {index} ({}): does not match the committed oracle deep list \
                         {expected:?}\n got hex: {got_hex}\nwant hex: {want_hex}",
                        row.key()
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The live-oracle cross-check (byte-exact, printer-free; fails loudly)
// ---------------------------------------------------------------------------

/// The lowercase hex of `term`'s `erlang:term_to_binary/1` image.
fn term_to_binary_hex(ctx: &mut BifContext<'_>, reg: &BifRegistry, term: Eterm) -> String {
    let image = reg
        .call("erlang", "term_to_binary", 1, ctx, &[term])
        .expect("erlang:term_to_binary/1");
    let (bytes, offset, size_bits) = bitstring_bytes(image).expect("a binary result");
    assert_eq!(offset, 0, "the image is a whole-byte binary");
    assert_eq!(size_bits % 8, 0, "the image is whole-byte aligned");
    let data = &bytes[..size_bits / 8];
    let mut out = String::with_capacity(data.len() * 2);
    for &byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Flattens a rendered character list to its text — used to read the `~w`
/// rendering of a subject back as Erlang source.
fn flatten_chars(term: Eterm) -> String {
    let mut out = String::new();
    let mut cursor = term;
    loop {
        if is_nil(cursor) {
            return out;
        }
        let head = cons_head(cursor).expect("a proper list of chars");
        if is_small(head) {
            let byte = u8::try_from(signed_value(head)).expect("a byte-sized char");
            out.push(byte as char);
        } else {
            out.push_str(&flatten_chars(head));
        }
        cursor = cons_tail(cursor).expect("a proper list of chars");
    }
}

/// The Erlang source of `subject`, derived from the **same term** the Rust half
/// renders — so the two halves of a case cannot describe different terms.
fn erl_subject(ctx: &mut BifContext<'_>, reg: &BifRegistry, subject: Eterm) -> String {
    let format_term = {
        let bytes: Vec<Eterm> = b"~w".iter().map(|&b| int(ctx, i64::from(b))).collect();
        list(ctx, &bytes)
    };
    let args_term = list(ctx, &[subject]);
    let rendered = reg
        .call("io_lib", "format", 2, ctx, &[format_term, args_term])
        .expect("io_lib:format(\"~w\", [Subject])");
    flatten_chars(rendered)
}

/// The `erl -eval` body for one chunk of cases: one lowercase-hex line per case,
/// the hex of `erlang:term_to_binary/1` of the call's result (or of the atom
/// `badarg` when the call raises — `try … catch error:badarg -> badarg end`, so a
/// raising case is byte-comparable without a stack trace).
///
/// The format strings and subject sources are ASCII (a binary is written as
/// `<<230,151,165>>`), so the eval string never carries a raw non-ASCII byte.
fn oracle_eval(chunk: &[(String, String, i64)]) -> String {
    let mut body = String::from(
        "Hex = fun(B) -> lists:flatten([io_lib:format(\"~2.16.0b\", [X]) \
         || X <- binary_to_list(B)]) end, ",
    );
    for (format, subject, limit) in chunk {
        body.push_str(&format!(
            "io:format(\"~s~n\", [Hex(erlang:term_to_binary((try \
             io_lib:format(\"{format}\", [{subject}], [{{chars_limit,{limit}}}]) \
             catch error:badarg -> badarg end)))]), ",
        ));
    }
    body.push_str("halt().");
    body
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs io_lib ~ts limited] $ERTS_ORACLE='{}' is not an existing \
             executable.",
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

/// Runs one `erl -noshell -eval <eval>` (the eval must `halt()`) and returns stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
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
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// How many cases one `erl -eval` invocation carries — the whole matrix would make
/// a single command line of a few hundred kilobytes.
const ORACLE_CHUNK: usize = 300;

/// Re-derives the **whole** matrix against the **live** oracle and requires the
/// `erlang:term_to_binary/1` image of every result to agree byte-for-byte.
///
/// A missing or failing oracle is a **failure**, never a silent pass.
#[test]
fn unlimited_and_limited_ts_cross_check_the_live_oracle() {
    let oracle = resolve_oracle().unwrap_or_else(|| {
        panic!(
            "the live-oracle cross-check for the limiting `~ts` arm requires a \
             system `erl` ($ERTS_ORACLE naming an executable, or one on $PATH). \
             Refusing to pass silently: install Erlang/OTP or set $ERTS_ORACLE."
        )
    });
    let reg = default_registry();
    let rows = golden_rows();
    assert!(
        !rows.is_empty(),
        "the live-oracle `~ts` corpus must not be empty"
    );
    // Build every case's arguments, derive the Erlang source of the very same
    // terms, and render the crate's own result — one fresh heap per case.
    let mut cases: Vec<(String, String, i64, String)> = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let arg = subject(&mut ctx, &row.subject);
        let source = erl_subject(&mut ctx, &reg, arg);
        let term = match format_limited_ts(&mut ctx, &reg, &row.format, row.limit, arg) {
            Ok(term) => term,
            Err(BeamError::BadArg) => atom(&mut ctx, "badarg"),
            Err(other) => panic!("case {}: unexpected error {other:?}", row.key()),
        };
        let hex = term_to_binary_hex(&mut ctx, &reg, term);
        cases.push((row.format.clone(), source, row.limit, hex));
    }
    let mut compared = 0usize;
    for (chunk_index, chunk) in cases.chunks(ORACLE_CHUNK).enumerate() {
        let eval_cases: Vec<(String, String, i64)> = chunk
            .iter()
            .map(|(format, source, limit, _)| (format.clone(), source.clone(), *limit))
            .collect();
        let eval = oracle_eval(&eval_cases);
        let stdout = run_erl(&oracle, &eval);
        let lines: Vec<&str> = stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(
            lines.len(),
            chunk.len(),
            "chunk {chunk_index}: the oracle printed {} non-empty lines for {} cases \
             — the generated `-eval` did not run every case:\n{stdout}",
            lines.len(),
            chunk.len(),
        );
        for ((format, source, limit, ours), want) in chunk.iter().zip(lines.iter()) {
            assert!(
                !want.trim().is_empty(),
                "case {format} of {source} at CL {limit}: the oracle line is empty"
            );
            assert_eq!(
                ours,
                want.trim(),
                "case {format} of {source} at CL {limit} disagrees with the live \
                 oracle (erlang:term_to_binary image)"
            );
            compared += 1;
        }
    }
    assert_eq!(
        compared,
        rows.len(),
        "every golden row must be compared against the live oracle"
    );
    assert_eq!(
        compared,
        MATRIX_ROWS,
        "the live-oracle cross-check must cover the whole {MATRIX_ROWS}-row matrix"
    );
}
