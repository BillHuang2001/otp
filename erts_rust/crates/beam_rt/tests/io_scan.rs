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

//! The **library-level io-device read path**, end to end at the **executable** boundary
//! and cross-checked against the live system `erl` oracle.
//!
//! `lib/stdlib/src/io.erl`'s `scan_erl_exprs/4`, `parse_erl_exprs/4` and
//! `lib/kernel/src/file.erl`'s `read/2` all reach the io device through `io:request/2`
//! (`io.erl:1421-1422`, `:1554-1564`), and `io.erl`'s `bc_req/3` probes
//! `net_kernel:dflag_unicode_io/1` on **every** request. That probe and the
//! paired `{file_reply|io_reply, ReplyAs, Reply}` reply tag are what let the request
//! complete at all: without them the caller's `receive` never matches the reply and the
//! call folds into the engine's typed block, so the read path is unreachable from library
//! code.
//!
//! `beam_rt`'s native `file_server_2` service serves the request
//! (`../../src/native/file_server.rs` + `get_until.rs`): the `{get_until, unicode, Prompt,
//! erl_scan, tokens, [Pos0, Options]}` arm runs the **real** `erl_scan:tokens/4` on the
//! engine and keeps its leftover in the device, exactly as `State#state.buf` does.
//!
//! # What is compared
//!
//! Every probe below is run **twice** — once under the built `beam_rt` binary
//! ([`CARGO_BIN_EXE_beam_rt`]) and once under the system `erl` — and the two stdouts must
//! be **equal**. The oracle is the expected value, so a divergence shows up as a mismatch
//! instead of being hardcoded away. Two probes (`dflag_unicode_io/1` on `self()`, and the
//! 5-byte `file:read/2`) additionally assert the exact value, so they still discriminate
//! without a toolchain.
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).
//!
//! # Note on the probe shape
//!
//! Each result is printed **immediately after** the request that produced it, in its own
//! `io:format/2` call. A `{get_until, …}` request runs Erlang code on the engine, and this
//! runtime does not yet keep an earlier request's answer valid across a later one, so
//! holding several scan results and printing them together reads a recycled region.

// The shared harness is compiled in whole, but this suite uses only its bounded runner and
// its toolchain discovery; the unused remainder is expected here — the same shape
// `tests/runtime_hooks.rs` uses.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

// ---------------------------------------------------------------------------------------
// The probes both runtimes are given
// ---------------------------------------------------------------------------------------

/// `io.erl`'s unicode-IO probe, which `bc_req/3` reads before **every** `io:request/2`.
const PROBE_DFLAG: &str =
    "io:format(\"~p~n\",[net_kernel:dflag_unicode_io(self())]), halt(0).";

/// The same probe with a non-pid argument: the oracle's `badarg`, caught so both runtimes
/// print the reason they raised.
const PROBE_DFLAG_NON_PID: &str = "R = try net_kernel:dflag_unicode_io(42) \
     catch error:E -> E end, io:format(\"~p~n\",[R]), halt(0).";

/// Opens the fixture read-only. `OPEN.1` is the `{ok, Device}` tuple the probes match.
const OPEN: &str = "{ok,D}=file:open(PATH,[read]),";

/// `file:read/2` on the fixture's first five bytes. The fixture is `-module(...)`-headed,
/// so the answer is the list `"-modu"` (the `[read]` mode is a **list** mode).
const PROBE_READ: &str = "{ok,D}=file:open(PATH,[read]), \
     io:format(\"~p~n\",[file:read(D,5)]), file:close(D), halt(0).";

/// `io:scan_erl_exprs/4` over the whole fixture: the `-module`, the `-export` and the
/// function form, then the `eof` read. Each result is printed as soon as it is produced.
const SCAN_BODY: &str = "A=io:scan_erl_exprs(D,\"\",{1,1},[]), io:format(\"~p~n\",[A]),\
     B=io:scan_erl_exprs(D,\"\",{1,1},[]), io:format(\"~p~n\",[B]),\
     C=io:scan_erl_exprs(D,\"\",{1,1},[]), io:format(\"~p~n\",[C]),\
     E=io:scan_erl_exprs(D,\"\",{1,1},[]), io:format(\"~p~n\",[E]),\
     file:close(D), halt(0).";

/// `io:parse_erl_exprs/4` over the same fixture and the same four reads.
///
/// `parse_erl_exprs/4` parses **expressions**, so the two attributes come back as
/// expressions (`-module(fixture)` is unary minus applied to a call) and the function form
/// is the `erl_parse` syntax error a form-only input has — both answers are the oracle's,
/// which is exactly the point of the comparison.
const PARSE_BODY: &str = "A=io:parse_erl_exprs(D,\"\",{1,1},[]), io:format(\"~p~n\",[A]),\
     B=io:parse_erl_exprs(D,\"\",{1,1},[]), io:format(\"~p~n\",[B]),\
     C=io:parse_erl_exprs(D,\"\",{1,1},[]), io:format(\"~p~n\",[C]),\
     E=io:parse_erl_exprs(D,\"\",{1,1},[]), io:format(\"~p~n\",[E]),\
     file:close(D), halt(0).";

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` under the shared bounded runner (`/dev/null` stdin, killed at
/// `support::PROBE_TIMEOUT`), failing loudly when the probe could not be run at all.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// Asserts this runtime ran `what` successfully and returns its stdout.
fn ours(what: &str, expr: &str) -> String {
    let run = beam_rt_eval(expr);
    assert_eq!(run.code, 0, "{what}: beam_rt exit {} — {}", run.code, run.err);
    assert!(
        !run.out.is_empty(),
        "{what}: beam_rt printed nothing — {}",
        run.err
    );
    run.out
}

/// Asserts the oracle renders `what` exactly as `ours_stdout` does. A missing oracle is a
/// loud skip (the caller has already asserted this runtime's own answer).
fn agrees_with_the_oracle(what: &str, expr: &str, ours_stdout: &str) {
    let Some(oracle) = support::require_oracle("io_scan") else {
        return;
    };
    let Some(theirs) = support::oracle_eval(&oracle, None, expr) else {
        eprintln!("warning: the oracle could not answer `{what}` — not compared");
        return;
    };
    assert_eq!(
        ours_stdout, theirs,
        "{what}: beam_rt and the system erl disagree"
    );
}

// ---------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_io_scan_<pid>_<n>_<label>`, unique per test and per run.
    fn new(label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_io_scan_{}_{unique}_{label}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The **Erlang string literal** for `path`: the temporary names this suite builds are
/// ASCII, which Rust's `Debug` and Erlang's string syntax quote identically.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII temporary path"))
}

/// The multi-line fixture the scan/parse/read probes walk: a module attribute, an export
/// attribute and a function form, one per line.
const FIXTURE: &str = "-module(fixture).\n-export([add/2]).\nadd(A, B) -> A + B.\n";

/// Writes [`FIXTURE`] into a fresh temporary directory and returns both.
fn fixture(label: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new(label);
    let path = dir.0.join("fixture.erl");
    fs::write(&path, FIXTURE).expect("write the fixture");
    (dir, path)
}

/// The four-line [`OPEN`] + `body` program with `PATH` bound to `path`.
fn program(path: &Path, body: &str) -> String {
    format!("{}{}", OPEN.replace("PATH", &literal(path)), body)
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// `net_kernel:dflag_unicode_io/1` on a local pid is the atom `true` — the probe `io.erl`'s
/// `bc_req/3` makes before every `io:request/2`. A non-pid is `badarg`.
#[test]
fn the_unicode_io_probe_answers_true_for_a_local_pid() {
    let out = ours("dflag_unicode_io(self())", PROBE_DFLAG);
    assert_eq!(out, "true", "a local pid's distribution entry is this node");
    agrees_with_the_oracle("dflag_unicode_io(self())", PROBE_DFLAG, &out);

    let out = ours("dflag_unicode_io(42)", PROBE_DFLAG_NON_PID);
    assert_eq!(out, "badarg", "a non-pid argument is the oracle's badarg");
    agrees_with_the_oracle("dflag_unicode_io(42)", PROBE_DFLAG_NON_PID, &out);
}

/// `file:read/2` — the `{get_chars, …}` io-device request reached the library level. The
/// fixture's first five bytes are `"-modu"`, which is also the pinned expected value.
#[test]
fn reading_the_fixture_matches_the_live_oracle() {
    let (_dir, path) = fixture("read");
    let expr = PROBE_READ.replace("PATH", &literal(&path));
    let out = ours("file:read/2", &expr);
    assert_eq!(out, "{ok,\"-modu\"}", "the fixture's first five bytes");
    agrees_with_the_oracle("file:read/2", &expr, &out);
}

/// `io:scan_erl_exprs/4` over the whole fixture: the `{get_until, …}` request runs the real
/// `erl_scan:tokens/4` on the engine, and the device's leftover carries the next form.
#[test]
fn scanning_the_fixture_matches_the_live_oracle() {
    let (_dir, path) = fixture("scan");
    let expr = program(&path, SCAN_BODY);
    let out = ours("io:scan_erl_exprs/4", &expr);
    assert!(
        out.starts_with("{ok,["),
        "the first form scans to `{{ok, Tokens, EndPos}}`, got {out}"
    );
    assert!(
        out.ends_with("{eof,{1,1}}"),
        "the exhausted device reports `{{eof, _}}`, got {out}"
    );
    agrees_with_the_oracle("io:scan_erl_exprs/4", &expr, &out);
}

/// `io:parse_erl_exprs/4` over the same fixture — the same request with `erl_parse` behind
/// the tokens.
#[test]
fn parsing_the_fixture_matches_the_live_oracle() {
    let (_dir, path) = fixture("parse");
    let expr = program(&path, PARSE_BODY);
    let out = ours("io:parse_erl_exprs/4", &expr);
    assert!(
        out.starts_with("{ok,[{op,{1,1},'-',"),
        "the module attribute parses as an expression, got {out}"
    );
    assert!(
        out.ends_with("{eof,{1,1}}"),
        "the exhausted device reports `{{eof, _}}`, got {out}"
    );
    agrees_with_the_oracle("io:parse_erl_exprs/4", &expr, &out);
}
