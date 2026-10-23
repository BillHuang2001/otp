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

//! `erts_internal:term_to_string/1,2` pinned against the **live** system `erl`, byte
//! for byte (contract A.7 — the OTP application contracts, the preloaded
//! `erts_internal` surface).
//!
//! # What is under test
//!
//! `term_to_string/1` is an Erlang wrapper in the preloaded `erts_internal.erl`
//! (`term_to_string(T) -> term_to_string(T, undefined)`); `/2` is a BIF
//! (`bif.c:4160-4193`). This runtime serves `/2` from its native floor
//! (`crate::native::erts_internal`), which renders through
//! `beam_bifs::print::c_printf::term_text` — `beam_bifs`' faithful port of the C's
//! `%T` (`erl_printf_term.c::print_term`) — over
//! `crate::format::term_text`/`Runtime::format_term_text`.
//!
//! So the observable contract is the **`%T`** renderer plus the C's argument rule, and
//! both differ from anything a `~p`/`~w` printer would answer:
//!
//! * `%T` writes no spaces around `=`/`=>` (`#{a=>1}`, `#recmod:r{ddlfa=1,ddlfb=2}`);
//! * a float is printf's `%e` with six fraction digits (`1.500000e+00`);
//! * a printable list is `"hello"`, never `[104,101,…]`;
//! * the `/2` **limit is a byte budget**, not a `~P` nesting depth: the renderer stops
//!   mid-term and appends the four bytes `"... "` — tail **including the trailing
//!   space** — abandoning a printable list's closing quote when it stops inside one;
//! * `undefined` is the C's `INT_MAX`/unlimited form, and the accepted numeric range is
//!   `1 < L < INT_MAX` — so `1`, `0`, a negative limit, `INT_MAX` itself and a
//!   non-numeric limit are all `error:badarg`.
//!
//! Every expectation below is the oracle's **own stdout bytes**, never a literal
//! written here: the suite compares the exact stdout image of one command line run by
//! both runtimes, and re-derives each structural claim from the oracle's bytes.
//!
//! # Why the fixture route, and why one `show/1` line per case
//!
//! A `-eval` action prints nothing of its own, so a fixture that prints exactly one
//! line per case gives a line-addressable image whose whole byte content is
//! comparable. Each case is wrapped in
//!
//! ```text
//! try erts_internal:term_to_string(T [, Limit]) catch error:R -> {error, R} end
//! ```
//!
//! and printed by `term_to_string_terms:show/1` — `io:format("~s\r\n")` for a string
//! (the rendering itself, verbatim) and `io:format("~p\r\n")` for `{error,badarg}`.
//! That is what turns the `badarg` arm into the deterministic **value** `{error,badarg}`
//! on both sides instead of a divergent stacktrace on stderr plus a divergent exit
//! code, and only the reason is bound (a stacktrace would be multi-line, so a case
//! would no longer be one line). `~s` rather than `erlang:display/1` because `display`
//! would re-`%T`-escape the text: the value under test *is* the text, so the comparison
//! is on the text and not on a quoted image of it.
//!
//! The terminator is the C's CRLF on both sides, [`crlf_lines`] requires it after
//! every line and rejects a bare CR/LF inside one, and the expected line count is
//! asserted on both sides — a renderer that fused two cases into one line, or dropped
//! one, cannot pass.
//!
//! # The two routes
//!
//! 1. **the fixture route** ([`term_to_string_matches_the_oracle_byte_for_byte`]) —
//!    `tests/fixtures/term_to_string_terms.erl`, compiled at test time by the system
//!    `erlc`, run on both runtimes as the same `.beam` through the same `-eval`
//!    expression. Its field atoms are `ddlfa`/`ddlfb`, which no OTP module pre-interns
//!    (see the fixture's docs): a native record stores its field keys sorted by
//!    ascending atom index, so only un-preinternable names make the stored order the
//!    declaration order on **both** VMs, and only then is the record line comparable.
//! 2. **the typed-gap route** ([`the_typed_gaps_exit_three_where_the_oracle_renders`])
//!    — the two arms this runtime cannot render at all (an identity-less `-eval`
//!    `Local` fun, and a foreign-node pid) are asserted to exit **3** here with exactly
//!    one `beam_rt: unsupported: <reason>` stderr line, **and** to be a real rendering
//!    on the oracle (exit 0, non-empty stdout) so the case cannot be vacuous.
//!
//! # Toolchain
//!
//! The oracle is resolved from the **filesystem** ([`support::require_oracle`], never by
//! running the tool) and the compiler as `erlc` beside it or on `$PATH`. A missing
//! toolchain is a **loud skip** ([`support::skip_loudly`]), never a silent pass; a
//! missing committed fixture **fails**. Every probe is bounded by
//! [`support::PROBE_TIMEOUT`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_rt::EXIT_UNSUPPORTED;

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "term_to_string_oracle";

/// The fixture module (its source lives in `tests/fixtures/`, its `.beam` is never committed).
const MODULE: &str = "term_to_string_terms";

/// The fixture's source, embedded so nothing is read relative to the test's CWD.
const FIXTURE_ERL: &str = include_str!("fixtures/term_to_string_terms.erl");

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The line terminator the fixture's `show/1` writes (`\r\n`), and the one the assertions
/// split on.
const CRLF: &[u8] = b"\r\n";

/// The C's truncation tail — three dots **and the space** (`erl_printf_term.c`'s
/// `"... "`), four bytes. A renderer that wrote `...` alone would fail every
/// [`assert_truncated`] call.
const TORN: &str = "... ";

/// The fixture's cases in `term_to_string_terms:all/0`'s emit order.
///
/// The **length** of this array is the number of CRLF lines every side must emit, so a
/// fixture edit that adds or drops a case must be reflected here (and the labels are how
/// the assertions below name a line — [`case_index`] refuses an unknown label, so a
/// reordering cannot silently re-point an assertion).
const CASES: [&str; 24] = [
    "atom/1",
    "float/1",
    "map/1",
    "string/1",
    "tuple/1",
    "binary/1",
    "record/1",
    "nested-list/1",
    "map-of-record/1",
    "tuple/2 undefined",
    "string/2 undefined",
    "tuple/2 limit 4",
    "string/2 limit 6",
    "string/2 limit 5",
    "string/2 limit 7",
    "record/2 limit 16",
    "record/2 limit 17",
    "record/2 limit 5",
    "tuple/2 limit 2147483646",
    "tuple/2 limit 2147483647",
    "tuple/2 limit 1",
    "tuple/2 limit 0",
    "tuple/2 limit -1",
    "tuple/2 limit foo",
];

/// The cases whose answer must be a **truncated** rendering: a byte budget that stops
/// inside the term, so the four-byte [`TORN`] tail is present.
///
/// The complement of this list must **not** carry the tail — the exact-fit budget
/// (`"hello"` at 7) and the widest accepted budget in particular.
const TRUNCATED: [&str; 6] = [
    "tuple/2 limit 4",
    "string/2 limit 6",
    "string/2 limit 5",
    "record/2 limit 16",
    "record/2 limit 17",
    "record/2 limit 5",
];

/// The cases that must answer the C's `badarg`, rendered as the fixture's
/// deterministic `{error,badarg}` value.
const BADARG: [&str; 5] = [
    "tuple/2 limit 2147483647",
    "tuple/2 limit 1",
    "tuple/2 limit 0",
    "tuple/2 limit -1",
    "tuple/2 limit foo",
];

/// The index in [`CASES`] of the case labelled `case`, or a panic when there is none.
///
/// Indexing by label rather than by a hand-counted integer is what keeps the assertions
/// tied to the fixture's emit order: a renamed or removed case fails loudly instead of
/// silently asserting about its neighbour.
fn case_index(case: &str) -> usize {
    CASES
        .iter()
        .position(|label| *label == case)
        .unwrap_or_else(|| panic!("no fixture case labelled `{case}` in {CASES:?}"))
}

// ---------------------------------------------------------------------------------------
// The oracle and the compiler (filesystem only — the oracle itself is never run to probe)
// ---------------------------------------------------------------------------------------

/// `erlc` beside the resolved oracle, else `erlc` on `$PATH` — the convention the crate's
/// other compiled-fixture suites use.
fn resolve_erlc(oracle: &Path) -> Option<PathBuf> {
    if let Some(dir) = oracle.parent() {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// A per-case scratch directory holding **only** the compiled fixture module (so it is
/// also a safe single `-pa` for the oracle).
struct Fixture {
    /// The scratch directory.
    dir: PathBuf,
}

/// Writes [`FIXTURE_ERL`] into a fresh scratch directory under `$TMPDIR` and compiles it
/// there with the system `erlc`.
///
/// Returns `None` after a loud skip when the toolchain cannot do it, and **panics** when
/// the committed source is missing or `erlc` rejects it (a repository defect, not a skip).
fn build_fixture(label: &str) -> Option<Fixture> {
    let oracle = support::require_oracle(SUITE)?;
    let Some(erlc) = resolve_erlc(&oracle) else {
        support::skip_loudly(SUITE, "no `erlc` beside the oracle and none on $PATH");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_term_to_string_{label}_{}_{n}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(SUITE, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let source = dir.join(format!("{MODULE}.erl"));
    assert!(
        !FIXTURE_ERL.trim().is_empty(),
        "the embedded `{MODULE}.erl` fixture source is empty"
    );
    std::fs::write(&source, FIXTURE_ERL).unwrap_or_else(|error| {
        panic!("cannot write the fixture source {}: {error}", source.display())
    });
    let output = std::process::Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output()
        .unwrap_or_else(|error| panic!("cannot run `{}`: {error}", erlc.display()));
    assert!(
        output.status.success(),
        "`{}` rejected the committed fixture {}: {}\n{}",
        erlc.display(),
        source.display(),
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
    let beam = dir.join(format!("{MODULE}.beam"));
    assert!(
        beam.is_file(),
        "`erlc` produced no {}; stderr:\n{}",
        beam.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(Fixture { dir })
}

// ---------------------------------------------------------------------------------------
// Bounded probes: this runtime, and the oracle
// ---------------------------------------------------------------------------------------

/// One finished CLI invocation: its exit code and its **raw** output bytes.
struct Cli {
    /// The exit code (`None` when the child was killed by a signal).
    code: Option<i32>,
    /// stdout, untrimmed — the compared observable.
    out: Vec<u8>,
    /// stderr, untrimmed.
    err: Vec<u8>,
}

impl Cli {
    /// Renders the run for a failure message.
    fn describe(&self, side: &str, args: &[String]) -> String {
        format!(
            "{side}: exit {:?}\n--- argv ---\n{args:?}\n--- stdout ({n} bytes) ---\n{l}\n--- stderr ---\n{e}",
            self.code,
            n = self.out.len(),
            l = String::from_utf8_lossy(&self.out),
            e = String::from_utf8_lossy(&self.err),
        )
    }
}

/// Runs `program` with `args` bounded by [`support::PROBE_TIMEOUT`], keeping the raw bytes.
fn run(program: &Path, args: &[String]) -> Option<Cli> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = support::run_bounded(program, &borrowed)?;
    Some(Cli {
        code: output.status.code(),
        out: output.stdout,
        err: output.stderr,
    })
}

/// Splits `bytes` into lines, requiring the fixture's `\r\n` terminator after **every**
/// line and rejecting a bare `\n`/`\r` anywhere in a line's text.
fn crlf_lines(bytes: &[u8], side: &str) -> Vec<Vec<u8>> {
    assert!(
        bytes.ends_with(CRLF),
        "{side}: the last line is not CRLF-terminated: {:?}",
        String::from_utf8_lossy(bytes)
    );
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\r' && bytes[i + 1] == b'\n' {
            let line = &bytes[start..i];
            assert!(
                !line.contains(&b'\r') && !line.contains(&b'\n'),
                "{side}: a line carries a bare CR/LF: {:?}",
                String::from_utf8_lossy(line)
            );
            lines.push(line.to_vec());
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    assert_eq!(
        start,
        bytes.len(),
        "{side}: bytes after the last CRLF: {:?}",
        String::from_utf8_lossy(&bytes[start..])
    );
    lines
}

/// The oracle (`erl`) and this runtime, on one command line: both must exit **0** with
/// empty stderr, and the comparison is the exact stdout bytes.
///
/// The expected line count is [`CASES`]'s length, which every side must produce — so a
/// renderer that fused two cases into one line, or dropped one, cannot pass.
fn compare_stdout(oracle: &Path, args: &[String]) -> (Vec<u8>, Vec<u8>) {
    let expected_lines = CASES.len();
    let theirs = run(Path::new(BINARY), args)
        .unwrap_or_else(|| panic!("{BINARY} did not exit within the probe bound"));
    let ours = run(oracle, args).unwrap_or_else(|| {
        panic!(
            "the oracle `{}` did not exit within the probe bound",
            oracle.display()
        )
    });
    assert_eq!(theirs.code, Some(0), "{}", theirs.describe("beam_rt", args));
    assert!(
        theirs.err.is_empty(),
        "beam_rt wrote to stderr:\n{}",
        theirs.describe("beam_rt", args)
    );
    assert_eq!(ours.code, Some(0), "{}", ours.describe("oracle", args));
    assert!(
        ours.err.is_empty(),
        "the oracle wrote to stderr:\n{}",
        ours.describe("oracle", args)
    );
    let their_lines = crlf_lines(&theirs.out, "beam_rt");
    let oracle_lines = crlf_lines(&ours.out, "oracle");
    assert_eq!(
        oracle_lines.len(),
        expected_lines,
        "the oracle emitted {} lines, not {expected_lines} (the fixture changed?):\n{}",
        oracle_lines.len(),
        String::from_utf8_lossy(&ours.out)
    );
    assert_eq!(
        their_lines.len(),
        expected_lines,
        "beam_rt emitted {} lines, not {expected_lines}:\n{}",
        their_lines.len(),
        String::from_utf8_lossy(&theirs.out)
    );
    assert_eq!(
        theirs.out,
        ours.out,
        "the rendered image must be byte-identical to the oracle's\n--- argv ---\n{args:?}\n--- beam_rt ({}) ---\n{:?}\n--- oracle ({}) ---\n{:?}\n--- as text ---\nbeam_rt: {}\noracle:  {}",
        theirs.out.len(),
        theirs.out,
        ours.out.len(),
        ours.out,
        String::from_utf8_lossy(&theirs.out),
        String::from_utf8_lossy(&ours.out),
    );
    (theirs.out, ours.out)
}

/// One line of a captured image as text (every leaf of the fixture is printable ASCII, so
/// the lossy conversion is exact here and the byte comparison above is the authority).
fn text(line: &[u8]) -> String {
    String::from_utf8_lossy(line).into_owned()
}

/// The text of the case labelled `case` in `image`, asserting the line is there.
fn case_line(image: &[u8], case: &str) -> String {
    let lines = crlf_lines(image, "image");
    text(&lines[case_index(case)])
}

/// Asserts `line` is a **truncated** rendering: it ends with the C's four-byte `"... "`
/// tail, with the space, and it is longer than the tail itself.
fn assert_truncated(line: &str, side: &str, case: &str) {
    assert!(
        line.ends_with(TORN),
        "{side}: `{case}` stops inside the term, so it must end with the C's four-byte \
         `\"... \"` tail (dots AND space): {line:?}"
    );
    assert!(
        line.len() > TORN.len(),
        "{side}: `{case}` is only the tail: {line:?}"
    );
}

/// Asserts `line` is **not** truncated — nothing was elided, so no tail is present.
fn assert_whole(line: &str, side: &str, case: &str) {
    assert!(
        !line.contains(TORN),
        "{side}: `{case}` renders in full, so it must not carry the `\"... \"` tail: {line:?}"
    );
}

/// The bytes `line` printed **before** the truncation tail — or the whole line when
/// nothing was elided.
///
/// This is the renderer's "prefix of the full rendering" as a value, so a budgeted case
/// can be compared against its own term's full rendering without either side being a
/// literal written here.
fn printed(line: &str) -> String {
    line.strip_suffix(TORN).unwrap_or(line).to_string()
}

/// Asserts `line` is printf's `%e` with **six** fraction digits (what `%T` uses for a
/// float), structurally — one leading digit, a `.`, six digits, `e`, a sign, at least two
/// exponent digits — and that it is neither the `~p` form (`1.5`) nor a five-digit
/// fraction.
fn assert_printf_e_float(line: &str, side: &str, case: &str) {
    let (mantissa, exponent) = line.split_once('e').unwrap_or_else(|| {
        panic!("{side}: `{case}` must be printf's `%e` form, got {line:?}")
    });
    let (integer, fraction) = mantissa.split_once('.').unwrap_or_else(|| {
        panic!("{side}: `{case}` has no fraction point, so it is not `%e`: {line:?}")
    });
    assert!(
        !integer.is_empty() && integer.chars().all(|c| c.is_ascii_digit()),
        "{side}: `{case}`'s mantissa integer part is not a digit run: {line:?}"
    );
    assert_eq!(
        fraction.len(),
        6,
        "{side}: `{case}`'s `%e` fraction is six digits (a `~p` form would be `1.5`): {line:?}"
    );
    assert!(
        fraction.chars().all(|c| c.is_ascii_digit()),
        "{side}: `{case}`'s fraction is a digit run: {line:?}"
    );
    let mut chars = exponent.chars();
    assert!(
        matches!(chars.next(), Some('+') | Some('-')),
        "{side}: `{case}`'s `%e` exponent carries a sign: {line:?}"
    );
    let digits: String = chars.collect();
    assert!(
        digits.len() >= 2 && digits.chars().all(|c| c.is_ascii_digit()),
        "{side}: `{case}`'s `%e` exponent is at least two digits: {line:?}"
    );
}

// ---------------------------------------------------------------------------------------
// Case 1 — the fixture route: the `%T` image, byte for byte
// ---------------------------------------------------------------------------------------

/// The whole `term_to_string/1,2` surface, cross-checked on the **same compiled fixture**.
///
/// The comparison is the exact stdout bytes (every case is one CRLF-terminated line, the
/// count is asserted on both sides). Beyond that, every structural claim is **re-derived
/// from the oracle's own bytes** on both sides — a claim written against a hand-typed
/// literal could agree with a wrong renderer:
///
/// * **the `%T` shape** — `#{a=>1}` with no spaces, `<<"xy">>` for a binary, `"hello"`
///   rather than `[104,101,…]`, `1.500000e+00` for a float, and a record as
///   `#recmod:r{ddlfa=1,ddlfb=2}` (`=` with no surrounding spaces);
/// * **the truncation tail** — the cases tagged in [`TRUNCATED`] each end with the four
///   bytes `"... "` *including the space*, and every other case carries no tail at all.
///   Two of them (`{a,1}` at 4, `"hello"` at 6) print **every** byte of the rendering and
///   still append it, so `"... "` alone cannot be read as "something was elided";
/// * **a truncated body is a prefix, never a re-render** — it is always a prefix of the
///   same term's `/1` rendering, and a larger budget never prints less;
/// * **the abandoned closing quote** — a budget that stops inside a printable list's
///   characters keeps the opening quote and never reaches the closing one;
/// * **the exact fit has no tail** — `"hello"` at 7 is the full rendering;
/// * **`undefined` is the unlimited form** — `/2 undefined` equals `/1` for both a tuple
///   and a printable list, and the widest accepted numeric budget renders in full too;
/// * **the `badarg` arms** — `INT_MAX` itself, `1`, `0`, `-1` and a non-numeric atom all
///   answer `{error,badarg}`.
#[test]
fn term_to_string_matches_the_oracle_byte_for_byte() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let Some(fixture) = build_fixture("terms") else {
        return;
    };
    let expr = format!("{MODULE}:all(), halt(0).");
    let args = vec![
        "-noshell".to_string(),
        "-pa".to_string(),
        fixture.dir.display().to_string(),
        "-eval".to_string(),
        expr,
    ];
    let (theirs, ours) = compare_stdout(&oracle, &args);

    for (side, image) in [("beam_rt", &theirs), ("oracle", &ours)] {
        // ---- the `%T` shape ------------------------------------------------
        let atom = case_line(image, "atom/1");
        assert_eq!(
            atom, "hello",
            "{side}: `%T` of an atom is the bare atom, with no quoting"
        );

        let float = case_line(image, "float/1");
        assert_printf_e_float(&float, side, "float/1");

        let map = case_line(image, "map/1");
        assert!(
            map.starts_with("#{") && map.ends_with('}'),
            "{side}: `%T` writes a map as `#{{…}}`: {map:?}"
        );
        assert!(
            map.contains("a=>1"),
            "{side}: a map entry is `key=>value` with no spaces: {map:?}"
        );
        assert!(
            !map.contains(' '),
            "{side}: `%T` puts no spaces in a map: {map:?}"
        );

        let string = case_line(image, "string/1");
        assert_eq!(
            string, "\"hello\"",
            "{side}: the printable-string rule, and explicitly not `[104,101,…]`"
        );

        let tuple = case_line(image, "tuple/1");
        assert_eq!(tuple, "{a,1}", "{side}: a tuple, no spaces");

        let binary = case_line(image, "binary/1");
        assert_eq!(
            binary, "<<\"xy\">>",
            "{side}: `%T` prints a binary as `<<…>>`, quoting a printable payload"
        );

        // ---- the native record ---------------------------------------------
        let record = case_line(image, "record/1");
        assert!(
            record.starts_with("#recmod:r{") && record.ends_with('}'),
            "{side}: `%T` writes a native record as `#Module:Name{{…}}`: {record:?}"
        );
        assert!(
            !record.contains(" = ") && !record.contains("= "),
            "{side}: a record field is `key=value` with no surrounding spaces: {record:?}"
        );
        assert!(
            record.contains("ddlfa=1") && record.contains("ddlfb=2"),
            "{side}: both fields are present with their values: {record:?}"
        );

        // ---- the nested terms ----------------------------------------------
        let nested = case_line(image, "nested-list/1");
        assert!(
            nested.starts_with('[') && nested.ends_with(']'),
            "{side}: a mixed nested list keeps its brackets: {nested:?}"
        );
        assert!(
            nested.contains("\"hi\"") && nested.contains("#{b=>2}"),
            "{side}: the nested map and printable list are rendered in place: {nested:?}"
        );
        assert!(
            nested.contains(&float),
            "{side}: the nested float renders exactly as the top-level one (`{float}`), \
             so one float renderer serves both: {nested:?}"
        );

        let map_of_record = case_line(image, "map-of-record/1");
        assert!(
            map_of_record.contains(&record),
            "{side}: a record nested in a list in a map renders exactly as the top-level \
             one (`{record}`): {map_of_record:?}"
        );

        // ---- `undefined` is the unlimited form ------------------------------
        assert_eq!(
            case_line(image, "tuple/2 undefined"),
            tuple,
            "{side}: `/2 undefined` is the C's `INT_MAX` form and equals `/1`"
        );
        assert_eq!(
            case_line(image, "string/2 undefined"),
            string,
            "{side}: `/2 undefined` equals `/1` for a printable list too"
        );

        // ---- the truncation tail -------------------------------------------
        for case in TRUNCATED {
            assert_truncated(&case_line(image, case), side, case);
        }
        for case in CASES {
            if !TRUNCATED.contains(&case) {
                assert_whole(&case_line(image, case), side, case);
            }
        }

        // ---- a truncated rendering is a PREFIX of the full one ---------------
        // The budgeted renderer stops early: it never reorders, substitutes or
        // re-renders. Both halves of every pair are the oracle's own bytes — the
        // `…/1` case is the full rendering of the very same term.
        for (case, full_case) in [
            ("tuple/2 limit 4", "tuple/1"),
            ("string/2 limit 5", "string/1"),
            ("string/2 limit 6", "string/1"),
            ("record/2 limit 5", "record/1"),
            ("record/2 limit 16", "record/1"),
            ("record/2 limit 17", "record/1"),
        ] {
            let torn = case_line(image, case);
            let body = printed(&torn);
            let whole = case_line(image, full_case);
            assert!(
                torn.ends_with(TORN),
                "{side}: `{case}` carries the tail, so `printed` stripped it: {torn:?}"
            );
            assert!(
                whole.starts_with(&body),
                "{side}: `{case}` stops inside `{full_case}`'s rendering instead of \
                 re-rendering it: body {body:?}, full {whole:?}"
            );
        }

        // ---- a larger budget never prints less ------------------------------
        // Along one term's ascending budgets the printed prefix only grows (and, at the
        // budget that reaches the end, the tail disappears — `assert_whole` above).
        let ladder = [
            "record/2 limit 5",
            "record/2 limit 16",
            "record/2 limit 17",
            "record/1",
        ];
        for pair in ladder.windows(2) {
            let small = printed(&case_line(image, pair[0]));
            let large = printed(&case_line(image, pair[1]));
            assert!(
                large.starts_with(&small),
                "{side}: the rendering at `{}` extends the one at `{}`: {small:?} then \
                 {large:?}",
                pair[1],
                pair[0]
            );
        }

        // ---- the abandoned closing quote, and the exhausted budget ----------
        // `show/1` printed the rendering verbatim, so a printable list's opening quote is
        // the first byte of its line and its closing quote the last. A budget that stops
        // **inside** the characters keeps the opening quote and never reaches the closing
        // one.
        assert!(
            string.starts_with('"') && string.ends_with('"'),
            "{side}: the full rendering of a printable list is quoted: {string:?}"
        );
        let inside = printed(&case_line(image, "string/2 limit 5"));
        assert!(
            inside.starts_with('"') && !inside.ends_with('"'),
            "{side}: a budget that stops inside the characters keeps the opening quote and \
             abandons the closing one: {inside:?}"
        );
        // One unit more prints the closing quote — and still carries the tail, because the
        // C appends `"... "` whenever the budget is exhausted, even where every byte of the
        // rendering was printed. Measured, and the reason `"... "` alone cannot be read as
        // "something was elided".
        assert_eq!(
            printed(&case_line(image, "string/2 limit 6")),
            string,
            "{side}: a budget of 6 prints the whole printable list"
        );

        // ---- the exact fit has no tail, and is the full rendering -----------
        let exact = case_line(image, "string/2 limit 7");
        assert_eq!(
            exact, string,
            "{side}: a budget landing past the end renders in full, with no tail"
        );

        // ---- the same shape for a tuple, which is not a character run --------
        assert_eq!(
            printed(&case_line(image, "tuple/2 limit 4")),
            tuple,
            "{side}: a budget of 4 prints the whole `{tuple}` and still appends the tail \
             — the tuple's structural steps are not one byte each"
        );

        // ---- a budget that stops inside the record's field list --------------
        for case in ["record/2 limit 16", "record/2 limit 17"] {
            let torn = case_line(image, case);
            let body = printed(&torn);
            assert!(
                torn.ends_with(TORN) && body.starts_with("#recmod:r{"),
                "{side}: `{case}` stopped inside the record, past its `#Module:Name{{` \
                 prefix: {torn:?}"
            );
            assert!(
                body.contains('=') && !body.ends_with('}'),
                "{side}: `{case}` stopped inside the field list, between `=` and `}}`: \
                 {torn:?}"
            );
        }
        assert_eq!(
            case_line(image, "record/2 limit 16"),
            case_line(image, "record/2 limit 17"),
            "{side}: both budgets stop at the same point inside the field list (the \
             record's key/value steps are not one byte each)"
        );

        // A budget that stops inside the record's *name* keeps the `#` prefix and is
        // shorter than the `#Module:Name{` prefix of the full rendering — so it cannot be
        // mistaken for a stop inside the field list.
        let short = printed(&case_line(image, "record/2 limit 5"));
        assert!(
            record.starts_with(&short) && short.len() < "#recmod:r{".len(),
            "{side}: a budget of 5 stops inside the record's name: {short:?} of {record:?}"
        );

        // ---- the widest accepted budget renders in full ----------------------
        let wide = case_line(image, "tuple/2 limit 2147483646");
        assert_eq!(
            wide, tuple,
            "{side}: the widest accepted budget (`INT_MAX - 1`) truncates nothing"
        );

        // ---- the `badarg` arms ----------------------------------------------
        for case in BADARG {
            assert_eq!(
                case_line(image, case),
                "{error,badarg}",
                "{side}: `{case}` is outside the C's `1 < L < INT_MAX` (or is not \
                 `undefined`), so the floor's `badarg` lands in the fixture's catch clause"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// Case 2 — the typed gaps: exit 3 here, a real rendering on the oracle
// ---------------------------------------------------------------------------------------

/// The arms this runtime refuses to render, and the C `%T` renders anyway.
///
/// `term_text` is a *faithful* renderer, so it declines rather than inventing: an
/// external pid/port/reference (its channel number is not derivable from the frozen
/// term) and a `Local` fun with no `LocalFunIdentity` seam are typed `unsupported`s.
/// The CLI's convention turns an `unsupported` raise into exit **3** with exactly one
/// `beam_rt: unsupported: <reason>` stderr line, so both arms are asserted that way —
/// and both are asserted to be a **real** rendering on the oracle (exit 0, non-empty
/// stdout), so the case cannot pass vacuously if the oracle ever stopped supporting it.
///
/// The two arms and how they reach `-eval`:
///
/// 1. `fun() -> ok end` — an `erl_eval`-created `Local` fun. Its identity (`num_free`,
///    `env`, the descriptor's uniq/index) is `erl_eval` internals that no seam can
///    derive, and fabricating one is forbidden, so this must stay a typed gap even
///    though `beam_rt` does install `LocalFunIdentity` for a *compiled* module's funs.
///    The oracle renders it as `#Fun<erl_eval.N.UNIQ>`.
/// 2. a **foreign-node** pid, built by `binary_to_term/1` from the hand-written
///    `NEW_PID_EXT` image [`FOREIGN_PID_ETF`] — no fixture can mint a remote pid, and
///    `list_to_pid("<a@b.0.1>")` is a `badarg` **on this runtime** (exit 1, `%T` never
///    reached), so the ETF image is the only spelling that expresses the arm. The
///    oracle renders it as `<NODE-HASH.ID.SERIAL>`.
///
/// A third shape is deliberately **not** here: `list_to_pid("<0.1.0>")` /
/// `list_to_ref("#Ref<0.0.0.5>")` are *internal* identity terms, which the renderer does
/// serve (`"<0.1.0>"`, `"#Ref<0.0.0.5>"`) — the gap is the foreign node, not pids or
/// references as such.
#[test]
fn the_typed_gaps_exit_three_where_the_oracle_renders() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    for (case, expression, oracle_prefix) in [
        (
            "an identity-less `erl_eval` Local fun",
            "erts_internal:term_to_string(fun() -> ok end)".to_string(),
            "#Fun<",
        ),
        (
            "a foreign-node pid from a NEW_PID_EXT image",
            format!("erts_internal:term_to_string(binary_to_term({FOREIGN_PID_ETF}))"),
            "<",
        ),
    ] {
        let expr = format!("io:format(\"~s\\r\\n\", [{expression}]), halt(0).");
        let args = vec![
            "-noshell".to_string(),
            "-eval".to_string(),
            expr.clone(),
        ];

        // ---- this runtime: exit 3, empty stdout, ONE `unsupported` line ----
        let ours = run(Path::new(BINARY), &args)
            .unwrap_or_else(|| panic!("{BINARY} did not exit within the probe bound"));
        assert_eq!(
            ours.code,
            Some(EXIT_UNSUPPORTED),
            "{case}: the renderer's typed gap is the CLI's `unsupported`, exit \
             {EXIT_UNSUPPORTED}\n{}",
            ours.describe("beam_rt", &args)
        );
        assert!(
            ours.out.is_empty(),
            "{case}: nothing may be printed before the raise — no partial rendering, no \
             fabricated `#Fun<…>`:\n{}",
            ours.describe("beam_rt", &args)
        );
        let stderr = String::from_utf8_lossy(&ours.err);
        let stderr_body = stderr.strip_suffix('\n').unwrap_or(&stderr);
        assert_eq!(
            stderr_body.lines().count(),
            1,
            "{case}: exactly one stderr line (no USAGE block, no stacktrace):\n{}",
            ours.describe("beam_rt", &args)
        );
        let reason = stderr_body
            .strip_prefix("beam_rt: unsupported: ")
            .unwrap_or_else(|| {
                panic!(
                    "{case}: the unsupported line is `beam_rt: unsupported: <reason>`, got \
                     {:?}",
                    stderr_body
                )
            });
        assert!(
            !reason.is_empty() && !reason.chars().any(char::is_whitespace),
            "{case}: the reason is a single token, got {reason:?}"
        );

        // ---- the oracle: the SAME expression is a real rendering -----------
        let theirs = run(&oracle, &args).unwrap_or_else(|| {
            panic!(
                "the oracle `{}` did not exit within the probe bound",
                oracle.display()
            )
        });
        assert_eq!(
            theirs.code,
            Some(0),
            "{case}: the oracle renders what this runtime declines, so the case is real\n{}",
            theirs.describe("oracle", &args)
        );
        assert!(
            theirs.err.is_empty(),
            "{case}: the oracle printed no diagnostic:\n{}",
            theirs.describe("oracle", &args)
        );
        let lines = crlf_lines(&theirs.out, "oracle");
        assert_eq!(
            lines.len(),
            1,
            "{case}: one rendered line on the oracle:\n{}",
            theirs.describe("oracle", &args)
        );
        let rendered = text(&lines[0]);
        assert!(
            rendered.starts_with(oracle_prefix) && rendered.ends_with('>'),
            "{case}: the oracle's rendering is the C `%T` form starting `{oracle_prefix}` \
             and closing `>`: {rendered:?}"
        );
    }
}

/// The `NEW_PID_EXT` (tag `88`) ETF image of a **foreign** pid this VM can never reach:
/// node `nm` as `ATOM_UTF8_EXT` (tag `118`, length 2), then the 4-byte id, serial and
/// creation words (`1`, `0`, `0`), big-endian — the layout `external.c` decodes.
///
/// Measured on the oracle: `binary_to_term` yields `<NODE-HASH.1.0>` (a foreign pid
/// renders its node hash, since no name is known for a node this VM has not talked to),
/// and `term_to_string/1` renders exactly that. Here the term decodes and the renderer
/// then declines it — the gap being the *rendering* of an external identity, not the
/// decode.
const FOREIGN_PID_ETF: &str = "<<131,88,118,0,2,110,109,0,0,0,1,0,0,0,0,0,0,0,0>>";
