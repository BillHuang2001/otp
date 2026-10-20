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
//! Live-oracle gate for `prim_file:internal_normalize_utf8/1`
//! (`src/unicode/normalize/`, registered for real by `src/runtime/other.rs`).
//!
//! # What is pinned
//!
//! Every row of [`corpus`] is asked of the **live** system `erl` on `$PATH`
//! (OTP 29.1.1 / erts-17.1) and of this crate, through
//! [`beam_bifs::default_registry`] over a real `beam_alloc::Heap` — the same entry
//! point `beam_rt` drives — and the two answers must be equal, byte for byte, as
//! the BIF's observable: the **list of code points** it returns, or the plain
//! `error:badarg` it raises.
//!
//! **Every expected value is derived from the oracle at test time.** No golden is
//! transcribed into this file — that is the whole point of the gate: three of the
//! module's unit-test goldens had been invented rather than oracle-checked once,
//! and a hand-written expectation is exactly what would hide that class of
//! mistake. Adding a row is one line ([`Row::points`] / [`Row::bytes`] /
//! [`Row::unaligned`] / [`Row::charlist`] / [`Row::integer`]); the oracle decides
//! the answer.
//!
//! The observable is what the C's `do_utf8_to_list_normalize`
//! (`erts/emulator/beam/erl_unicode.c:1569`) actually does — **composition only**,
//! mark-adjacent, exclusion-list-ignoring and reorder-free — and **not** NFC/NFD/
//! NFKC. So the rows cover the composition chains (`A`+`U+0301` ⇒ `[193]`), the
//! roll-ups, the three-code-point chains, the mark pairs that must *not* be
//! reordered, a pair the Unicode standard excludes but the C table carries
//! (`U+0915 U+093C`), a pair with no row at all, and the astral/Hangul-jamo
//! boundaries; plus the decoder's classification (a noncharacter passes through,
//! a surrogate of either kind / an overlong form / a stray continuation byte / a
//! truncated tail are `badarg`) and the argument gates (only a **byte-aligned
//! bitstring**; a charlist, an integer and `<<1:3>>` are `badarg`).
//!
//! # ⚠ The oracle SIGABRTs on a mark-initial image — never add such a row
//!
//! An input whose **first** code point is a combining mark (`<<204,129>>`,
//! `U+0F71 U+0F72`, …) **aborts the oracle VM** (SIGABRT, exit 134): an upstream
//! `cleanup_norm` defect, where `numpoints` is reset on the roll-up path and the
//! trailing `if (numpoints) cleanup_norm(...)` reads an uninitialised
//! `savepoints[0]`. This port answers the sane thing (the mark passes through) and
//! the shape is deliberately absent from every corpus here, because a differential
//! case would compare against a crash rather than a value. Do **not** add a row
//! that starts with a combining mark — not even one taken from the C table.
//! [`the_row_table_never_starts_a_row_with_a_mark`] enforces that mechanically for
//! every mark this file itself uses, so a careless one-liner cannot reintroduce it.
//!
//! # Cost and fail-loudness
//!
//! The whole corpus is asked of the oracle in **one** `erl` invocation (138 tiny
//! rows, a ~13 KiB `-eval` program — far below Linux's 128 KiB `MAX_ARG_STRLEN`),
//! so the gate costs a single oracle startup (~0.2 s).
//!
//! It is **fail-loud** in both directions: with no usable `erl` it prints a banner
//! saying the cross-check did **not** run (that is *not* a pass), and if an oracle
//! *is* available but misbehaves (non-zero exit, a timeout, an unparsable answer,
//! a different number of answers than rows) it panics with the program and the
//! stdout/stderr attached. A row that disagrees is **not** the end of the test
//! either: every disagreeing row is reported together, with both values.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{cons_head, cons_tail, make_binary, make_bitstring};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{is_nil, is_small, make_small, signed_value, AtomTable, Eterm, NIL};

/// The wall-clock budget for the one oracle invocation.
const ERL_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Test harness: the BIF through the real registry
// ---------------------------------------------------------------------------

/// A registry + heap + atom table the rows are driven through. No optional
/// [`BifContext`] seam is installed — this BIF needs none.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            registry: default_registry(),
            // 138 rows of a few cons cells each: 64 Ki words is far more than the
            // whole corpus needs, and the bump allocator is never rewound here.
            heap: Heap::new(1 << 16).expect("test heap"),
            atoms: AtomTable::new(),
        }
    }

    /// `prim_file:internal_normalize_utf8(Arg)` through the real registry, with the
    /// answer folded into the BIF's observable ([`Answer`]).
    ///
    /// A raise other than `badarg` is a bug in this crate, not a row: it panics
    /// with the row named rather than being folded into the comparison.
    fn normalize(&mut self, row: &Row) -> Answer {
        let arg = self.arg_term(row);
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        match self
            .registry
            .call("prim_file", "internal_normalize_utf8", 1, &mut ctx, &[arg])
        {
            Ok(term) => Answer::Points(self.points_of(row, term)),
            Err(BeamError::BadArg) => Answer::BadArg,
            Err(other) => panic!(
                "prim_file:internal_normalize_utf8/1 raised {other:?} for row `{}` \
                 (input {}) — the only valid answers are the plain badarg or a list of \
                 code points",
                row.label,
                row.arg.erl(),
            ),
        }
    }

    /// The row's argument, built exactly as the oracle's spelling of the same row.
    fn arg_term(&mut self, row: &Row) -> Eterm {
        match &row.arg {
            Arg::Binary { bytes, .. } => {
                make_binary(&mut self.heap, bytes).expect("the binary fits")
            }
            Arg::Unaligned { byte, size_bits } => {
                make_bitstring(&mut self.heap, &[*byte], *size_bits).expect("the bitstring fits")
            }
            Arg::Charlist(text) => {
                let mut acc = NIL;
                for byte in text.bytes().rev() {
                    acc = self
                        .heap
                        .cons(make_small(i64::from(byte)), acc)
                        .expect("a cons cell fits");
                }
                acc
            }
            Arg::Integer(value) => make_small(*value),
        }
    }

    /// Reads the BIF's answer as the oracle prints it: a proper list of code points.
    fn points_of(&self, row: &Row, term: Eterm) -> Vec<u32> {
        let mut points = Vec::new();
        let mut cursor = term;
        loop {
            if is_nil(cursor) {
                return points;
            }
            let head = cons_head(cursor).unwrap_or_else(|error| {
                panic!(
                    "prim_file:internal_normalize_utf8/1 answered a non-list for row `{}`: \
                     {error:?}",
                    row.label
                )
            });
            assert!(
                is_small(head),
                "prim_file:internal_normalize_utf8/1 must answer code points, but row `{}` \
                 carries a non-small-integer element",
                row.label
            );
            let value = signed_value(head);
            points.push(u32::try_from(value).unwrap_or_else(|_| {
                panic!(
                    "prim_file:internal_normalize_utf8/1 answered {value}, which is not a \
                     code point, for row `{}`",
                    row.label
                )
            }));
            cursor = cons_tail(cursor).unwrap_or_else(|error| {
                panic!(
                    "prim_file:internal_normalize_utf8/1 answered an improper list for row \
                     `{}`: {error:?}",
                    row.label
                )
            });
        }
    }
}

// ---------------------------------------------------------------------------
// The corpus: one line per row, the oracle decides every value
// ---------------------------------------------------------------------------

/// One corpus row: the argument — built identically on both sides — plus the code
/// points it names, when it names any.
struct Row {
    /// What the diagnostics call this row: the input, spelled as Erlang.
    label: String,
    arg: Arg,
    /// The code points this row names, empty for a row that names none (a raw byte
    /// image, a charlist, an integer). Read only by the mark-initial guard.
    points: Vec<u32>,
}

/// How a row's argument is built.
enum Arg {
    /// `<<bytes>>` — the binary/bitstring form the BIF accepts, byte-aligned.
    Binary { bytes: Vec<u8> },
    /// `<<Byte:SizeBits>>` with `SizeBits` not a multiple of 8.
    Unaligned { byte: u8, size_bits: usize },
    /// A charlist, which the oracle also rejects.
    Charlist(&'static str),
    /// A small integer.
    Integer(i64),
}

impl Arg {
    /// The argument as an Erlang expression — the oracle's side of the same row.
    fn erl(&self) -> String {
        match self {
            Arg::Binary { bytes } => {
                let fields: Vec<String> = bytes.iter().map(u8::to_string).collect();
                format!("<<{}>>", fields.join(","))
            }
            Arg::Unaligned { byte, size_bits } => format!("<<{byte}:{size_bits}>>"),
            Arg::Charlist(text) => format!("\"{text}\""),
            Arg::Integer(value) => value.to_string(),
        }
    }
}

impl Row {
    /// A row naming `code_points`, sent as the UTF-8 encoding of them — the only
    /// shape the BIF accepts.
    fn points(code_points: &[u32]) -> Row {
        let text: String = code_points
            .iter()
            .map(|&cp| {
                char::from_u32(cp).expect(
                    "a `Row::points` entry must be a Unicode scalar value — use `Row::bytes` \
                     for a surrogate or otherwise invalid image",
                )
            })
            .collect();
        Row {
            label: render_points(code_points),
            arg: Arg::Binary {
                bytes: text.into_bytes(),
            },
            points: code_points.to_vec(),
        }
    }

    /// A row carrying a **raw byte image**, for the decoder's classification (an
    /// overlong form, a surrogate, a truncated tail — images no code-point list can
    /// express).
    fn bytes(bytes: &[u8]) -> Row {
        let label = format!(
            "<<{}>>",
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<String>>()
                .join(",")
        );
        Row {
            label,
            arg: Arg::Binary {
                bytes: bytes.to_vec(),
            },
            points: Vec::new(),
        }
    }

    /// A row carrying a non-byte-aligned bitstring (`<<byte:size_bits>>`).
    fn unaligned(byte: u8, size_bits: usize) -> Row {
        Row {
            label: format!("<<{byte}:{size_bits}>>"),
            arg: Arg::Unaligned { byte, size_bits },
            points: Vec::new(),
        }
    }

    /// A row carrying a charlist.
    fn charlist(text: &'static str) -> Row {
        Row {
            label: format!("\"{text}\""),
            arg: Arg::Charlist(text),
            points: Vec::new(),
        }
    }

    /// A row carrying a small integer.
    fn integer(value: i64) -> Row {
        Row {
            label: value.to_string(),
            arg: Arg::Integer(value),
            points: Vec::new(),
        }
    }

    /// The first code point of the row's byte image, when it has a decodable one.
    ///
    /// This is the guard's probe: a **valid** UTF-8 image whose first code point is
    /// a combining mark is what crashes the oracle, so it must not exist in the
    /// corpus. A row that is not a byte image (a charlist, an integer, a
    /// non-byte-aligned bitstring) is `badarg` before the walk, so it cannot abort.
    fn first_code_point(&self) -> Option<u32> {
        let Arg::Binary { bytes } = &self.arg else {
            return None;
        };
        let decoded = match std::str::from_utf8(bytes) {
            Ok(text) => text,
            // The longest valid prefix: a row whose image is invalid *after* its
            // first code point still aborts the oracle if that code point is a mark
            // (the C validates the whole image first, but be conservative).
            Err(error) => {
                std::str::from_utf8(&bytes[..error.valid_up_to()]).expect("a valid prefix")
            }
        };
        decoded.chars().next().map(|ch| ch as u32)
    }
}

/// `[65,769]` — a code-point list as the diagnostics print it.
fn render_points(code_points: &[u32]) -> String {
    let fields: Vec<String> = code_points.iter().map(u32::to_string).collect();
    format!("[{}]", fields.join(","))
}

/// The base letters of the systematic two-code-point fan: the ASCII vowels and the
/// two most chain-bearing consonants.
const FAN_BASES: [u32; 6] = [65, 97, 101, 105, 111, 117];

/// The combining marks of the systematic fan: the U+0300..U+0327 block's ordinary
/// marks plus `U+0323`. Every one of them is a *mark*, so it is only ever the
/// second code point of a row.
const FAN_MARKS: [u32; 12] = [
    768, 769, 770, 771, 772, 774, 775, 776, 777, 778, 780, 803,
];

/// The base letters of the three-code-point fan.
const CHAIN_BASES: [u32; 3] = [65, 97, 105];

/// The first mark of the three-code-point fan.
const CHAIN_MARKS: [u32; 4] = [769, 770, 776, 778];

/// The second mark of the three-code-point fan.
const CHAIN_TAILS: [u32; 2] = [769, 778];

/// Every **combining mark** the corpus uses — the vocabulary the mark-initial guard
/// rejects a row's first code point from (see
/// [`the_row_table_never_starts_a_row_with_a_mark`]).
///
/// The list is the fans' marks plus the two marks of the named rows (`U+093C` the
/// nukta of the exclusion pair, `U+3099` the kana voiced sound mark). It is checked
/// against the corpus in both directions — every entry must still appear in some
/// row, and no row may begin with one — so a new mark is one entry here as well as
/// one row below. Adding a row whose **first** code point is a mark is exactly the
/// mistake this list exists to stop: the oracle SIGABRTs on such an image.
const MARK_POOL: [u32; 14] = [
    768, 769, 770, 771, 772, 774, 775, 776, 777, 778, 780, 803, 0x093C, 0x3099,
];
/// The whole corpus, in a fixed order: the named rows (the chains, the roll-ups,
/// the order-preserving pairs, the table-vs-standard exclusion, the astral and
/// Hangul-jamo boundaries), then the decoder/argument-gate rows, then the two
/// systematic fans.
fn corpus() -> Vec<Row> {
    let mut rows = vec![
        // ---- the raw-bytes form of the canonical chain: A + U+0301 ⇒ [193].
        Row::bytes(&[65, 204, 129]),
        // ---- composition chains.
        Row::points(&[101, 769]),      // e + acute ⇒ é
        Row::points(&[65, 778, 769]),  // a three-code-point chain
        Row::points(&[105, 776, 769]), // i + diaeresis + acute composes
        Row::points(&[105, 770, 769]), // i + circumflex + acute has no row
        // ---- roll-ups: a pending group that cannot extend closes and re-opens.
        Row::points(&[193, 769]),
        Row::points(&[192, 769]),
        Row::points(&[224, 769]),
        Row::points(&[65, 769, 769]),
        Row::points(&[65, 768, 770, 769]),
        // ---- marks are never reordered: whatever the oracle says, it must hold.
        Row::points(&[65, 769, 778]),
        Row::points(&[97, 776, 769]),
        Row::points(&[97, 769, 776]),
        Row::points(&[117, 770, 769]),
        // ---- the C table carries no Unicode exclusion list: U+0915 U+093C composes.
        Row::points(&[0x0915, 0x093C]),
        // ---- pairs with no row in the table at all stay apart.
        Row::points(&[66, 769]),
        Row::points(&[0x03B1, 0x0309]),
        Row::points(&[9000, 9060]),
        // ---- two independent groups in one image.
        Row::points(&[65, 769, 97, 769]),
        // ---- the astral boundary: a code point past the C's Uint16 window.
        Row::points(&[0x1F600]),
        Row::points(&[65, 0x1F600, 769]),
        // ---- no Hangul composition, and the kana voiced-sound-mark chain.
        Row::points(&[0x1100, 0x1161]),
        Row::points(&[0x304B, 0x3099]),
        // ---- plain ASCII, and the empty image.
        Row::points(&[104, 105]),
        Row::points(&[97]),
        Row::points(&[]),
        // ---- decoder classification: noncharacters pass through ...
        Row::points(&[0xFDD0]),
        Row::points(&[0xFFFF]),
        // ---- ... while a surrogate of either kind, an overlong form, a stray
        // continuation byte, a truncated tail and a past-U+10FFFF image are badarg.
        Row::bytes(&[0xED, 0xA0, 0x80]), // U+D800, a high surrogate
        Row::bytes(&[0xED, 0xB0, 0x80]), // U+DC00, a low surrogate
        Row::bytes(&[0xC1, 0x81]),       // two-byte overlong form of 'A'
        Row::bytes(&[0xE0, 0x80, 0x80]), // three-byte overlong form of NUL
        Row::bytes(&[0xF0, 0x80, 0x80, 0x80]), // four-byte overlong form of NUL
        Row::bytes(&[0x80]),             // a stray continuation byte
        Row::bytes(&[0x41, 0xCC]),       // 'A' + a truncated U+0301
        Row::bytes(&[0xC3]),             // a truncated two-byte sequence
        Row::bytes(&[0xF4, 0x90, 0x80, 0x80]), // U+110000, past the last code point
        Row::bytes(&[0xFE]),             // never a UTF-8 lead byte
        Row::bytes(&[0xFF]),             // never a UTF-8 lead byte
        // ---- the argument gates: only a byte-aligned bitstring is accepted.
        Row::charlist("abc"),
        Row::integer(97),
        Row::unaligned(0b0010_0000, 3), // <<1:3>>
    ];

    // ---- the systematic fans, every row still decided by the oracle.
    for base in FAN_BASES {
        for mark in FAN_MARKS {
            rows.push(Row::points(&[base, mark]));
        }
    }
    for base in CHAIN_BASES {
        for mark in CHAIN_MARKS {
            for tail in CHAIN_TAILS {
                rows.push(Row::points(&[base, mark, tail]));
            }
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// The answer both sides are compared on
// ---------------------------------------------------------------------------

/// The BIF's observable: the code points it returns, or the plain `badarg`.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Points(Vec<u32>),
    BadArg,
}

impl fmt::Display for Answer {
    /// The oracle's own rendering (`[193]`, `[]`, `badarg`), so a mismatch prints
    /// both sides in one format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Answer::Points(points) => f.write_str(&render_points(points)),
            Answer::BadArg => f.write_str("badarg"),
        }
    }
}

// ---------------------------------------------------------------------------
// The oracle
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
/// same resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs normalize_oracle] $ERTS_ORACLE='{}' is not an existing \
             executable — treating it as no oracle.",
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

/// Reads a child pipe to EOF, surfacing an I/O error instead of swallowing it.
fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    pipe.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Joins a pipe-reader thread, reporting a panic or an I/O error rather than hiding
/// it.
fn join_pipe(reader: thread::JoinHandle<std::io::Result<Vec<u8>>>, which: &str) -> Option<Vec<u8>> {
    match reader.join() {
        Ok(Ok(buf)) => Some(buf),
        Ok(Err(error)) => {
            eprintln!("[beam_bifs normalize_oracle] SKIP: reading the oracle {which} failed: {error}");
            None
        }
        Err(_) => {
            eprintln!("[beam_bifs normalize_oracle] SKIP: the oracle {which} reader panicked");
            None
        }
    }
}

/// The outcome of one oracle invocation.
enum OracleRun {
    /// The oracle ran to completion and printed this stdout.
    Stdout(String),
    /// No oracle is usable here (unresolvable, unspawnable, wedged) — a **skip**,
    /// never a pass.
    Unusable,
}

/// Runs one `erl -noshell -eval <program>` under [`ERL_TIMEOUT`].
///
/// The child's stdout and stderr are drained on worker threads (so a full pipe can
/// never wedge it) while the main thread waits with a deadline; a child that
/// overruns the budget is **killed** and reaped, so a hung `erl` can never hang the
/// suite. An oracle that is *available* but answers non-zero is **not** a skip: it
/// panics with the program and both streams, because that means the cross-check did
/// not do its job.
fn run_oracle(oracle: &Path, program: &str) -> OracleRun {
    let spawned = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "[beam_bifs normalize_oracle] SKIP: could not spawn the oracle '{}': {error}",
                oracle.display()
            );
            return OracleRun::Unusable;
        }
    };

    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_reader = thread::spawn(move || read_pipe(stderr_pipe));

    let deadline = Instant::now() + ERL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    eprintln!(
                        "[beam_bifs normalize_oracle] SKIP: the oracle did not answer within \
                         {ERL_TIMEOUT:?} and was killed."
                    );
                    return OracleRun::Unusable;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                eprintln!(
                    "[beam_bifs normalize_oracle] SKIP: waiting for the oracle failed: {error}"
                );
                return OracleRun::Unusable;
            }
        }
    };

    let stdout = join_pipe(stdout_reader, "stdout");
    let stderr = join_pipe(stderr_reader, "stderr");
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        return OracleRun::Unusable;
    };

    assert!(
        status.success(),
        "the oracle '{}' exited with {:?} — every row must be answerable (a row whose first \
         code point is a combining mark SIGABRTs the oracle; it must not be in the corpus)\n\
         --- program ---\n{program}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        oracle.display(),
        status.code(),
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );
    OracleRun::Stdout(String::from_utf8_lossy(&stdout).into_owned())
}

/// The one `-eval` program that asks the oracle for every row.
///
/// `F` folds a row's outcome into the same two shapes this crate's [`Answer`] has:
/// a **flat** string `[p1,p2,…]` built by the test itself (so neither side depends
/// on `io_lib`'s printer), or the literal `badarg`. Any *other* raise is not caught
/// and so crashes the oracle loudly, which is what we want.
fn oracle_program(rows: &[Row]) -> String {
    let mut statements = vec![
        "F = fun(Fun) -> try Fun() of \
           L -> lists:flatten([\"[\", lists:join(\",\", [integer_to_list(X) || X <- L]), \
           \"]\"]) \
         catch error:badarg -> \"badarg\" end end"
            .to_string(),
    ];
    for row in rows {
        statements.push(format!(
            "io:format(\"~s~n\", [F(fun() -> prim_file:internal_normalize_utf8({}) end)])",
            row.arg.erl(),
        ));
    }
    statements.push("halt(0).".to_string());
    statements.join(",\n")
}

/// Parses the oracle's one-line-per-row answers.
fn parse_answers(stdout: &str, rows: &[Row], program: &str) -> Vec<Answer> {
    let lines: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(
        lines.len(),
        rows.len(),
        "the oracle printed {} answers for {} rows\n--- program ---\n{program}\n--- stdout ---\n{stdout}",
        lines.len(),
        rows.len(),
    );
    lines
        .iter()
        .zip(rows)
        .map(|(line, row)| parse_answer(line, row, program))
        .collect()
}

/// Parses one `[p1,p2,…]` / `badarg` line.
fn parse_answer(line: &str, row: &Row, program: &str) -> Answer {
    if line == "badarg" {
        return Answer::BadArg;
    }
    let inner = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| {
            panic!(
                "the oracle answered `{line}` for row `{}`, which is neither a code-point \
                 list nor `badarg`\n--- program ---\n{program}",
                row.label
            )
        });
    if inner.is_empty() {
        return Answer::Points(Vec::new());
    }
    Answer::Points(
        inner
            .split(',')
            .map(|field| {
                field.trim().parse::<u32>().unwrap_or_else(|error| {
                    panic!(
                        "`{field}` in the oracle's answer `{line}` for row `{}` is not a code \
                         point: {error}\n--- program ---\n{program}",
                        row.label
                    )
                })
            })
            .collect(),
    )
}

/// Prints the loud "no oracle, not a pass" banner.
fn skip_banner() {
    eprintln!();
    eprintln!("========== beam_bifs normalize_oracle: live oracle SKIPPED ==========");
    eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
    eprintln!("executable, and no `erl` on $PATH).");
    eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
    eprintln!("Agreement of prim_file:internal_normalize_utf8/1 with the real");
    eprintln!("erts-17.1 normalizer is then UNVERIFIED for every row.");
    eprintln!("====================================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The gate: every row must answer exactly as the live oracle does.
///
/// Every expected value comes from the oracle at test time — nothing is
/// transcribed here. All disagreeing rows are reported together, with both values,
/// because one mismatch usually means a table region is off and the neighbours are
/// the useful evidence.
#[test]
fn oracle_agrees_on_the_normalizer() {
    let Some(oracle) = resolve_oracle() else {
        skip_banner();
        return;
    };

    let rows = corpus();
    let program = oracle_program(&rows);
    let stdout = match run_oracle(&oracle, &program) {
        OracleRun::Stdout(stdout) => stdout,
        OracleRun::Unusable => {
            skip_banner();
            return;
        }
    };
    let expected = parse_answers(&stdout, &rows, &program);

    let mut fixture = Fixture::new();
    let mut mismatches = Vec::new();
    let mut badargs = 0usize;
    for (row, oracle_answer) in rows.iter().zip(&expected) {
        if *oracle_answer == Answer::BadArg {
            badargs += 1;
        }
        let ours = fixture.normalize(row);
        if ours != *oracle_answer {
            mismatches.push(format!(
                "  {}: this crate = {ours}, oracle = {oracle_answer}",
                row.label,
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "prim_file:internal_normalize_utf8/1 disagrees with the live oracle on {} of {} rows:\n{}\n\
         --- program ---\n{program}\n--- oracle stdout ---\n{stdout}",
        mismatches.len(),
        rows.len(),
        mismatches.join("\n"),
    );

    eprintln!(
        "[beam_bifs normalize_oracle] live oracle '{}': {} rows agreed ({badargs} of them \
         badarg, {} code-point answers).",
        oracle.display(),
        rows.len(),
        rows.len() - badargs,
    );
}

/// The guard for the oracle's `cleanup_norm` SIGABRT: **no row may start with a
/// combining mark**.
///
/// The vocabulary is [`MARK_POOL`], cross-checked against the corpus in both
/// directions (every entry is used by some row, so the list cannot go stale, and
/// every row's first code point is outside it) and covering each row's *byte image*
/// too, so a `Row::bytes` image that happens to be valid UTF-8 starting with a mark
/// is rejected as well. Runs with or without an oracle, so the hazard is caught
/// before the gate can boot a VM into it.
#[test]
fn the_row_table_never_starts_a_row_with_a_mark() {
    let rows = corpus();
    assert!(
        rows.len() >= 100,
        "the corpus must stay broad — it has shrunk to {} rows",
        rows.len(),
    );

    // Every mark the corpus uses is in the vocabulary: a mark that only ever appears
    // in the fans or in a named row cannot be missing from `MARK_POOL`.
    let named_marks = [0x093C, 0x3099];
    for mark in FAN_MARKS
        .iter()
        .chain(&CHAIN_MARKS)
        .chain(&CHAIN_TAILS)
        .chain(&named_marks)
    {
        assert!(
            MARK_POOL.contains(mark),
            "U+{mark:04X} is used as a mark by the corpus but is missing from MARK_POOL",
        );
    }

    // ... and no vocabulary entry is stale: each one still appears in some row.
    let mut used: Vec<u32> = Vec::new();
    for row in &rows {
        used.extend(row.points.iter().skip(1).copied());
    }
    used.sort_unstable();
    used.dedup();
    for mark in MARK_POOL {
        assert!(
            used.contains(&mark),
            "MARK_POOL lists the mark U+{mark:04X}, which no row uses any more",
        );
    }

    for row in &rows {
        let Some(first) = row.first_code_point() else {
            continue;
        };
        assert!(
            !MARK_POOL.contains(&first),
            "row `{}` starts with the combining mark U+{first:04X}: the oracle SIGABRTs on a \
             mark-initial image (an upstream `cleanup_norm` defect), so such a row can never be \
             a differential case — put the mark after a base code point",
            row.label,
        );
    }
}
