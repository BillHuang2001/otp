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

//! `erlang:display/1` pinned against the **live** system `erl`, byte for byte.
//!
//! The key is `beam_bifs`' (`term_conv::display_1`): the C's
//! `erts_printf("%.*T\n", INT_MAX, Term)` — the **`%T`** renderer, not `~p`/`~w`, followed by
//! the `\r\n` its `write_f_add_cr` (`erts/lib_src/common/erl_printf.c`) turns the `\n` into —
//! written through the same `OutputSink` seam `io:format/1,2` uses. This suite is what holds
//! that claim against reality: every expectation is the oracle's **own stdout bytes**, never a
//! literal written here.
//!
//! # What the `%T` renderer buys, and why the CRLF is the proof
//!
//! `%T` and `~p` differ observably and a `~p` renderer would agree on nothing below:
//!
//! * a native record is `#Module:Name{key=value,…}` — `=` with **no** spaces, fields in the
//!   definition's stored key order — where `~p` writes the plain tuple `{r,1,"xy"}`;
//! * a map is `#{k=>v}` with no spaces;
//! * a float is printf's `%e` with six fraction digits (`1.500000e+00`);
//! * a printable list is `"xy"`, never `[120,121]`.
//!
//! The **terminator** is the cheap discriminator between the two renderers: `%T`'s line ends
//! `\r\n`, a `~p`/`io:fwrite` line ends with a bare `\n`. [`CRLF`] is asserted on every line of
//! every probe, on both sides, so an implementation that printed the right text through the
//! wrong renderer still fails.
//!
//! # The oracle is measured, never assumed
//!
//! The comparison is the **exact stdout bytes** of one command line run by both runtimes, so
//! the terminator is compared too, and the oracle is never represented by a hard-coded string.
//! Measured on the differential oracle (OTP 29.1.1 / `erts-17.1`): `erlang:display(42)` on a
//! **pipe** writes the four bytes `34 32 0D 0A` — the C's `add_cr_to_stdout` flag is not
//! conditional on a terminal, so CRLF is the oracle's pipe behaviour as well, and
//! [`display_writes_the_oracles_crlf_terminated_line`] is the case that states it outright.
//!
//! # The two routes
//!
//! 1. **the fixture route** ([`display_matches_the_oracle_byte_for_byte`]) — a module compiled
//!    at test time by the system `erlc` whose `all/0` displays one term per line: a native
//!    record, a map, a float, an atom and a printable list. Both runtimes run the **same**
//!    `.beam` through the **same** `-eval` expression, so nothing library-level can differ
//!    (the fixture's only library call is `erlang:display/1` itself).
//! 2. **the expression route** ([`display_of_eval_expressions_matches_the_oracle_byte_for_byte`])
//!    — `erlang:display(…)` literals written straight into `-eval`, i.e. the differential
//!    harness' own probe shape, with no fixture at all.
//!
//! A `-eval` action prints nothing of its own, so both routes' stdout is the `display` output
//! alone: no result line to strip, and the whole byte image is comparable.
//!
//! # Toolchain
//!
//! The oracle is resolved from the **filesystem** ([`support::require_oracle`], never by
//! running the tool) and the compiler as `erlc` beside it or on `$PATH`. A missing toolchain is
//! a **loud skip** ([`support::skip_loudly`]), never a silent pass; a missing committed fixture
//! **fails**. Every probe is bounded by `support::PROBE_TIMEOUT`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "display_oracle";

/// The fixture module (its source lives in `tests/fixtures/`, its `.beam` is never committed).
const MODULE: &str = "display_terms";

/// The fixture's source, embedded so nothing is read relative to the test's CWD.
const FIXTURE_ERL: &str = include_str!("fixtures/display_terms.erl");

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The line terminator the C writes (`erts_printf` → `write_f_add_cr`), and the one a `~p`
/// renderer does **not** write.
const CRLF: &[u8] = b"\r\n";

/// The fixture's displayed terms, in the order `display_terms:all/0` prints them —
/// `[record, map, float, atom, printable list]`.
const FIXTURE_LINES: usize = 5;

// ---------------------------------------------------------------------------------------
// The oracle and the compiler (filesystem only — the oracle itself is never run to probe)
// ---------------------------------------------------------------------------------------

/// `erlc` beside the resolved oracle, else `erlc` on `$PATH` — the convention the crate's other
/// compiled-fixture suites use.
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

/// A per-case scratch directory holding **only** the compiled fixture module (so it is also a
/// safe single `-pa` for the oracle).
struct Fixture {
    /// The scratch directory.
    dir: PathBuf,
}

/// Writes [`FIXTURE_ERL`] into a fresh scratch directory under `$TMPDIR` and compiles it there
/// with the system `erlc`.
///
/// Returns `None` after a loud skip when the toolchain cannot do it, and **panics** when the
/// committed source is missing or `erlc` rejects it (a repository defect, not a skip).
fn build_fixture(label: &str) -> Option<Fixture> {
    let oracle = support::require_oracle(SUITE)?;
    let Some(erlc) = resolve_erlc(&oracle) else {
        support::skip_loudly(SUITE, "no `erlc` beside the oracle and none on $PATH");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_display_{label}_{}_{n}",
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

/// Splits `bytes` into lines, requiring the C's `\r\n` terminator after **every** line and
/// rejecting a bare `\n`/`\r` anywhere in a line's text.
///
/// This is the assertion that separates the `%T` renderer from a `~p` one: `%T`'s line ends
/// `\r\n` (`write_f_add_cr`), `io:fwrite`/`~p`'s ends with a lone `\n`.
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

/// The oracle (`erl`) and this runtime, on one command line: both must exit **0** with empty
/// stderr, and the comparison is the exact stdout bytes.
///
/// `expected_lines` is the number of CRLF-terminated lines the route emits, which every side
/// must produce — so a renderer that fused two terms into one line, or dropped a terminator,
/// cannot pass.
fn compare_stdout(oracle: &Path, args: &[String], expected_lines: usize) -> (Vec<u8>, Vec<u8>) {
    let theirs = run(Path::new(BINARY), args)
        .unwrap_or_else(|| panic!("{} did not exit within the probe bound", BINARY));
    let ours = run(oracle, args).unwrap_or_else(|| {
        panic!(
            "the oracle `{}` did not exit within the probe bound",
            oracle.display()
        )
    });
    assert_eq!(
        theirs.code,
        Some(0),
        "{}",
        theirs.describe("beam_rt", args)
    );
    assert!(
        theirs.err.is_empty(),
        "beam_rt wrote to stderr:\n{}",
        theirs.describe("beam_rt", args)
    );
    assert_eq!(
        ours.code,
        Some(0),
        "{}",
        ours.describe("oracle", args)
    );
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
        "the oracle emitted {} lines, not {expected_lines} (the fixture/expression changed?):\n{}",
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
        "the display output must be byte-identical to the oracle's\n--- argv ---\n{args:?}\n--- beam_rt ({}) ---\n{:?}\n--- oracle ({}) ---\n{:?}\n--- as text ---\nbeam_rt: {}\noracle:  {}",
        theirs.out.len(),
        theirs.out,
        ours.out.len(),
        ours.out,
        String::from_utf8_lossy(&theirs.out),
        String::from_utf8_lossy(&ours.out),
    );
    (theirs.out, ours.out)
}

/// One line of a captured image as text (every leaf of the fixture is printable ASCII).
fn text(line: &[u8]) -> String {
    String::from_utf8_lossy(line).into_owned()
}

// ---------------------------------------------------------------------------------------
// Case 1 — the fixture route: record, map, float, atom, printable list
// ---------------------------------------------------------------------------------------

/// The five `%T` shapes, cross-checked on the **same compiled fixture**.
///
/// Beyond the byte-exact comparison this asserts the `%T`-specific structure the shapes carry,
/// on **both** sides (so the claims are the oracle's, re-derived from its own bytes):
///
/// * the record line is `#display_terms:r{…}` — the module-qualified `%T` form, `=` with no
///   surrounding spaces, and both `zz=1` and `yy="xy"` present (the field *order* is the
///   definition's stored key order and is deliberately not pinned here);
/// * the map line is `#{…}` with `=>` and **no** spaces: `alpha=>1`, `beta=>[1,2]`;
/// * the float line is printf's `%e` at six fraction digits: `1.500000e+00`;
/// * the atom line is the bare atom: `hello`;
/// * the printable-list line is `"xy"` — the printable-string rule, and explicitly **not**
///   `[120,121]`.
#[test]
fn display_matches_the_oracle_byte_for_byte() {
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
    let (theirs, ours) = compare_stdout(&oracle, &args, FIXTURE_LINES);
    for (side, image) in [("beam_rt", &theirs), ("oracle", &ours)] {
        let lines = crlf_lines(image, side);
        let record = text(&lines[0]);
        assert!(
            record.starts_with("#display_terms:r{") && record.ends_with('}'),
            "{side}: the native record is `%T`'s module-qualified form: {record}"
        );
        assert!(
            !record.contains(" = ") && !record.contains("= "),
            "{side}: `%T` writes `key=value` with no spaces: {record}"
        );
        assert!(
            record.contains("zz=1") && record.contains("yy=\"xy\""),
            "{side}: both fields are present with their values: {record}"
        );

        let map = text(&lines[1]);
        assert_eq!(
            map, "#{alpha=>1,beta=>[1,2]}",
            "{side}: `%T` writes a map as `#{{k=>v}}` with no spaces"
        );

        assert_eq!(
            text(&lines[2]),
            "1.500000e+00",
            "{side}: `%T` writes a float as printf `%e` with six fraction digits"
        );
        assert_eq!(text(&lines[3]), "hello", "{side}: a bare atom");
        assert_eq!(
            text(&lines[4]),
            "\"xy\"",
            "{side}: the printable-string rule, not `[120,121]`"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Case 2 — the expression route: the differential harness' own probe shape
// ---------------------------------------------------------------------------------------

/// `erlang:display(…)` literals straight into `-eval`, with no fixture at all — the shape the
/// compatibility gate's probes use.
#[test]
fn display_of_eval_expressions_matches_the_oracle_byte_for_byte() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let expr = concat!(
        "erlang:display(#{k => 1}), ",
        "erlang:display(1.5), ",
        "erlang:display(\"xy\"), ",
        "erlang:display(hello), ",
        "halt(0)."
    )
    .to_string();
    let args = vec![
        "-noshell".to_string(),
        "-eval".to_string(),
        expr,
    ];
    let (theirs, _ours) = compare_stdout(&oracle, &args, 4);
    let lines = crlf_lines(&theirs, "beam_rt");
    assert_eq!(text(&lines[0]), "#{k=>1}", "a map, no spaces");
    assert_eq!(text(&lines[1]), "1.500000e+00", "a float, six digits");
    assert_eq!(text(&lines[2]), "\"xy\"", "a printable list");
    assert_eq!(text(&lines[3]), "hello", "an atom");
}

// ---------------------------------------------------------------------------------------
// Case 3 — the terminator itself
// ---------------------------------------------------------------------------------------

/// The terminator, stated outright: `erlang:display(42)` writes the four bytes `34 32 0D 0A` —
/// `%T` + the C's CRLF — on the oracle and here alike.
///
/// Measured, not assumed: `erts/lib_src/common/erl_printf.c`'s `write_f_add_cr` turns every
/// `\n` into `\r\n` whenever `erts_printf_add_cr_to_stdout` is set, and
/// `erts/emulator/sys/unix/sys.c` sets it unconditionally — so the oracle writes CRLF even when
/// stdout is a **pipe** (verified: `erl -noshell -eval 'erlang:display(42), halt(0).' | xxd` ⇒
/// `3432 0d0a`). If a future toolchain ever emitted a bare `\n` on a pipe, this case fails
/// naming the terminator instead of quietly agreeing with a CRLF-less implementation.
#[test]
fn display_writes_the_oracles_crlf_terminated_line() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let args = vec![
        "-noshell".to_string(),
        "-eval".to_string(),
        "erlang:display(42), halt(0).".to_string(),
    ];
    let (theirs, ours) = compare_stdout(&oracle, &args, 1);
    assert_eq!(
        ours,
        b"42\r\n".to_vec(),
        "the oracle's `erlang:display(42)` on a pipe is `%T` + CRLF"
    );
    assert_eq!(
        theirs,
        b"42\r\n".to_vec(),
        "this runtime writes the oracle's own four bytes"
    );
}
