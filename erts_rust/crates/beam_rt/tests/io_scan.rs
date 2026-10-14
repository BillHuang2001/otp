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
//! Every probe below is run under the built `beam_rt` binary ([`CARGO_BIN_EXE_beam_rt`]) and
//! (where the toolchain allows) under the system `erl`, and the printed lines must be
//! **equal**. The oracle is the expected value, so a divergence shows up as a mismatch
//! instead of being hardcoded away. Several probes additionally assert the **exact** value
//! (`dflag_unicode_io/1` on `self()`, the 5-byte `file:read/2`, and every buffered-read
//! sequence line below), so they still discriminate without a toolchain.
//!
//! # Two routes, one compiled function
//!
//! The `-eval` probes drive the **pumped** route: `Runtime::eval` runs on the root process and
//! pumps the scheduler, so an io-device `receive` is resumed inside the same call.
//!
//! The buffered-read sequences — `file:read/2` interleaved with `file:read_line/1` and
//! `file:position/2` — are additionally driven through `run <fixture.beam> ioseq:<fun>/1`, a
//! genuine **scheduled** process on the real scheduler/poller. Both routes call the *same*
//! compiled function, so the only difference between them is the execution route. Each
//! sequence line is asserted equal to a **pinned** value on **all three** of the scheduled
//! `run` line, the `-eval` line and the live oracle's line, so a divergence confined to one
//! route is a mismatch rather than a hardcoded-away agreement.
//!
//! The fixture module is compiled **at test time** with the system `erlc` (resolved the way
//! `tests/scheduled_io_device.rs` does: a sibling of the resolved oracle first, then `$PATH`);
//! it is never a committed `.beam`.
//!
//! A missing `erl` **or** `erlc` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).
//!
//! # Note on the probe shape
//!
//! Every probe prints each result **immediately after** the request that produced it, in its
//! own `io:format/2` call, except the deferred one below.
//!
//! A result may also be **held** across later requests: the `{get_until, …}` request runs
//! Erlang code on the engine and its answer is copied onto the requesting process's own young
//! heap, and this runtime keeps such an answer valid across later deliveries. The runtime
//! marks the process **relocation-exempt** for the whole `-eval` driver (and for the nested
//! machines a cross-module call or a fun application runs on), so a later delivery that
//! cannot fit the receiver's heap goes through the heap-fragment fallback instead of
//! collecting — a delivery-driven collection publishes only the process's own containers and
//! would recycle a term the evaluator still holds.
//! [`a_scan_result_held_across_later_requests_matches_the_live_oracle`] is the regression
//! probe for exactly that.

// The shared harness is compiled in whole, but this suite uses only its bounded runner and
// its toolchain discovery; the unused remainder is expected here — the same shape
// `tests/runtime_hooks.rs` uses.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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

/// The **deferred** scan: three `io:scan_erl_exprs/4` requests are issued back to back and
/// only `A` — the result of the **first** — is printed, once every request has completed.
///
/// This is the discriminating shape for the relocation-exempt scope: `A` is copied onto the
/// requesting process's young heap by the first reply and then held in the evaluator's
/// environment while two further replies are delivered to the same process. A later delivery
/// that does not fit would collect that heap, publishing only the process's own containers,
/// and recycle the words `A` points at — so the runtime keeps the process relocation-exempt
/// for the whole `-eval` driver, and the held term survives for the `~p` printer to render
/// the token list the oracle prints.
const HOLD_AND_PRINT_FIRST_BODY: &str = "A=io:scan_erl_exprs(D,\"\",{1,1},[]),\
     B=io:scan_erl_exprs(D,\"\",{1,1},[]),\
     C=io:scan_erl_exprs(D,\"\",{1,1},[]),\
     io:format(\"~p~n\",[A]), file:close(D), halt(0).";

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

/// A result **held across later requests** stays valid: three `io:scan_erl_exprs/4` calls on
/// one device, then the **first** result printed.
///
/// The three replies are copied onto the requesting process's young heap, and the evaluator
/// keeps `A` in a Rust local, outside the process root set. Without the relocation-exempt
/// scope a later reply's delivery collects that heap and recycles `A`'s words, and the printer
/// then reports `badarg` instead of the token list — the oracle prints the first form's tokens
/// either way.
#[test]
fn a_scan_result_held_across_later_requests_matches_the_live_oracle() {
    let (_dir, path) = fixture("hold");
    let expr = program(&path, HOLD_AND_PRINT_FIRST_BODY);
    let out = ours("a held io:scan_erl_exprs/4 result", &expr);
    assert!(
        out.starts_with("{ok,[{'-',{1,1}}"),
        "the first form's tokens, not a recycled region, got {out}"
    );
    assert!(
        out.ends_with("{2,1}}"),
        "the first form's end position, got {out}"
    );
    agrees_with_the_oracle("a held io:scan_erl_exprs/4 result", &expr, &out);
}

// ---------------------------------------------------------------------------------------
// The buffered-read sequence fixture (compiled at test time — never committed as a `.beam`)
// ---------------------------------------------------------------------------------------

/// The fixture module, compiled with `erlc` into a scratch dir. One export per read
/// sequence; each returns the tuple of results it observed on **one** device.
///
/// `mixed/1` interleaves `read/2` with `read_line/1` (the buffered path: `reply_get_chars`
/// consumes the device's stashed read-ahead leftover before reading the OS file, and
/// `read_line/1` drops the newline it consumed); `line_then_read/1` exercises a `read/2` that
/// must resume past a buffered line; `position_cur/1` and `position_cur_2/1` exercise
/// `file:position/2` over a device holding a stashed buffer (the arm answers the **logical**
/// position and clears the buffer on a successful seek); `at_eof/1` is the negative control —
/// a genuine `eof` still answers `eof`.
const SEQUENCE_SOURCE: &str = "\
-module(ioseq).
-export([mixed/1, line_then_read/1, position_cur/1, position_cur_2/1, at_eof/1]).

mixed(Path) ->
    {ok, F} = file:open(Path, [read]),
    A = file:read(F, 6),
    B = file:read_line(F),
    C = file:read(F, 4),
    ok = file:close(F),
    {A, B, C}.

line_then_read(Path) ->
    {ok, F} = file:open(Path, [read]),
    A = file:read_line(F),
    B = file:read(F, 100),
    ok = file:close(F),
    {A, B}.

position_cur(Path) ->
    {ok, F} = file:open(Path, [read]),
    A = file:read_line(F),
    P = file:position(F, cur),
    B = file:read(F, 4),
    ok = file:close(F),
    {A, P, B}.

position_cur_2(Path) ->
    {ok, F} = file:open(Path, [read]),
    A = file:read_line(F),
    P = file:position(F, {cur, 2}),
    B = file:read(F, 4),
    ok = file:close(F),
    {A, P, B}.

at_eof(Path) ->
    {ok, F} = file:open(Path, [read]),
    A = file:read(F, 16),
    B = file:read(F, 4),
    ok = file:close(F),
    {A, B}.
";

/// The data file every sequence reads: 16 bytes, two newline-terminated lines and a
/// newline-free tail.
const SEQUENCE_DATA: &str = "line1\nline2\nrest";

/// The pinned `mixed/1` line: `read/2` (6) → `read_line/1` → `read/2` (4). `file:read/2` on a
/// `[read]` **list**-mode device answers a string; `~w` renders it as its integer list.
const PINNED_MIXED: &str =
    "{{ok,[108,105,110,101,49,10]},{ok,[108,105,110,101,50,10]},{ok,[114,101,115,116]}}";

/// The pinned `line_then_read/1` line: `read_line/1` consumes `line1\n`, so the `read/2`
/// returns the rest of the file from `line2\n` on.
const PINNED_LINE_THEN_READ: &str =
    "{{ok,[108,105,110,101,49,10]},{ok,[108,105,110,101,50,10,114,101,115,116]}}";

/// The pinned `position_cur/1` line: after `read_line/1` the **logical** position is 6 (end of
/// `line1\n`), and the following `read/2` resumes there.
const PINNED_POSITION_CUR: &str =
    "{{ok,[108,105,110,101,49,10]},{ok,6},{ok,[108,105,110,101]}}";

/// The pinned `position_cur_2/1` line: `{cur, 2}` lands two bytes into `line2\n`, i.e.
/// logical position 8.
const PINNED_POSITION_CUR_2: &str =
    "{{ok,[108,105,110,101,49,10]},{ok,8},{ok,[110,101,50,10]}}";

/// The pinned `at_eof/1` line: the first `read/2` drains the 16-byte file and the second is a
/// genuine `eof` — the negative control that a buffered device still reports `eof`.
const PINNED_AT_EOF: &str =
    "{{ok,[108,105,110,101,49,10,108,105,110,101,50,10,114,101,115,116]},eof}";

/// The compiled fixture plus the data file it reads, in one scratch directory (removed on
/// drop). The directory holds no `lists.beam`, so it is a safe single `-pa` for the oracle.
struct SequenceFixture {
    dir: TempDir,
    beam: PathBuf,
    data: PathBuf,
}

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`
/// — the same resolution `tests/scheduled_io_device.rs` uses.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// Writes [`SEQUENCE_SOURCE`] and [`SEQUENCE_DATA`] into a fresh scratch directory and
/// compiles the module with `erlc`; `None` with a loud skip when the toolchain cannot.
fn build_sequence_fixture(erlc: &Path, label: &str) -> Option<SequenceFixture> {
    let dir = TempDir::new(label);
    let source = dir.0.join("ioseq.erl");
    fs::write(&source, SEQUENCE_SOURCE).expect("write the sequence fixture source");
    let data = dir.0.join("data");
    fs::write(&data, SEQUENCE_DATA).expect("write the sequence data file");
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir.0)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly("io_scan", &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            "io_scan",
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let beam = dir.0.join("ioseq.beam");
    if !beam.is_file() {
        support::skip_loudly("io_scan", &format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(SequenceFixture { dir, beam, data })
}

/// The toolchain gate shared by the sequence tests: the resolved oracle and the compiled
/// fixture, or `None` after a loud skip.
fn sequence_fixture_or_skip(label: &str) -> Option<(PathBuf, SequenceFixture)> {
    let oracle = support::require_oracle("io_scan")?;
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly("io_scan", "no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    let fixture = build_sequence_fixture(&erlc, label)?;
    Some((oracle, fixture))
}

/// `beam_rt -noshell run <beam> ioseq:<function>/1 "<data>"` — the **scheduled** process route.
///
/// `-noshell`: every non-`-version` action now drives the default boot script (A.9), so a booted
/// node prints the OTP banner to stdout unless it is head-less — and the oracle side of the
/// comparison probes with `erl -noshell …`.
fn beam_rt_run(beam: &Path, function: &str, data: &Path) -> Run {
    let spec = format!("ioseq:{function}/1");
    let beam = beam.to_str().expect("an ASCII fixture path");
    let data = literal(data);
    run(Path::new(BINARY), &["-noshell", "run", beam, &spec, &data])
}

/// `beam_rt -noshell -pa <dir> -eval <expr>` — the **pumped** `-eval` route, loading the
/// scratch dir so the same compiled function is reachable.
fn beam_rt_eval_with_path(dir: &Path, expr: &str) -> Run {
    let dir = dir.to_str().expect("an ASCII scratch path");
    run(Path::new(BINARY), &["-noshell", "-pa", dir, "-eval", expr])
}

/// The `-eval`-route program for `ioseq:<function>/1`, printed with `~w` so the line matches
/// the `run` action's own printer (only `~p` line-wraps).
fn eval_program(function: &str, data: &Path) -> String {
    format!(
        "io:format(\"~w~n\",[ioseq:{function}({})]), halt(0).",
        literal(data)
    )
}

/// The live oracle's line for `ioseq:<function>/1`, loaded from the same scratch dir.
fn oracle_line(oracle: &Path, dir: &Path, function: &str, data: &Path) -> String {
    support::oracle_eval(oracle, Some(dir), &eval_program(function, data))
        .unwrap_or_else(|| panic!("the live oracle could not answer ioseq:{function}/1"))
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Runs `ioseq:<function>/1` on **both** routes (scheduled `run` and pumped `-eval`) and
/// under the live oracle, asserting all three print exactly `expected`.
///
/// `expected` is pinned, so the probe discriminates even where the oracle comparison cannot
/// run; the oracle then proves the pinned value **is** the reference answer.
fn assert_sequence_agrees(oracle: &Path, fixture: &SequenceFixture, function: &str, expected: &str) {
    let scheduled = beam_rt_run(&fixture.beam, function, &fixture.data);
    assert_eq!(
        scheduled.code, 0,
        "ioseq:{function}/1 on a scheduled process exited {}: {}",
        scheduled.code, scheduled.err
    );
    assert_eq!(
        scheduled.out, expected,
        "the scheduled `run` line for ioseq:{function}/1"
    );

    let pumped = beam_rt_eval_with_path(&fixture.dir.0, &eval_program(function, &fixture.data));
    assert_eq!(
        pumped.code, 0,
        "ioseq:{function}/1 on the -eval route exited {}: {}",
        pumped.code, pumped.err
    );
    assert_eq!(pumped.out, expected, "the -eval line for ioseq:{function}/1");

    assert_eq!(
        oracle_line(oracle, &fixture.dir.0, function, &fixture.data),
        expected,
        "the pinned value must be the live oracle's ioseq:{function}/1 line"
    );
}

/// `file:read/2` → `file:read_line/1` → `file:read/2` on one device: the `read_line/1`
/// consumes the newline it saw, so the trailing `read/2` resumes exactly after it — the mixed
/// buffered-read shape, on both routes and the oracle.
#[test]
fn a_mixed_read_read_line_read_sequence_matches_the_live_oracle() {
    let Some((oracle, fixture)) = sequence_fixture_or_skip("seq_mixed") else {
        return;
    };
    assert_sequence_agrees(&oracle, &fixture, "mixed", PINNED_MIXED);
}

/// `file:read_line/1` then a large `file:read/2`: the `read/2` drains the rest of the file,
/// stashed leftover and all.
#[test]
fn a_read_line_then_read_sequence_matches_the_live_oracle() {
    let Some((oracle, fixture)) = sequence_fixture_or_skip("seq_line_read") else {
        return;
    };
    assert_sequence_agrees(&oracle, &fixture, "line_then_read", PINNED_LINE_THEN_READ);
}

/// A bare `file:position(F, cur)` after a `read_line/1` reports the **logical** position (past
/// the consumed line), and the following `read/2` resumes there.
#[test]
fn a_position_cur_after_a_read_line_matches_the_live_oracle() {
    let Some((oracle, fixture)) = sequence_fixture_or_skip("seq_pos_cur") else {
        return;
    };
    assert_sequence_agrees(&oracle, &fixture, "position_cur", PINNED_POSITION_CUR);
}

/// `file:position(F, {cur, 2})` after a `read_line/1` lands two bytes into the next line, and
/// the following `read/2` resumes there.
#[test]
fn a_position_cur_2_after_a_read_line_matches_the_live_oracle() {
    let Some((oracle, fixture)) = sequence_fixture_or_skip("seq_pos_cur2") else {
        return;
    };
    assert_sequence_agrees(&oracle, &fixture, "position_cur_2", PINNED_POSITION_CUR_2);
}

/// The negative control: a device that has genuinely reached end of file still answers `eof`
/// to a further `read/2` — the buffered path never fabricates data.
#[test]
fn a_genuine_eof_after_a_full_read_matches_the_live_oracle() {
    let Some((oracle, fixture)) = sequence_fixture_or_skip("seq_eof") else {
        return;
    };
    assert_sequence_agrees(&oracle, &fixture, "at_eof", PINNED_AT_EOF);
}
