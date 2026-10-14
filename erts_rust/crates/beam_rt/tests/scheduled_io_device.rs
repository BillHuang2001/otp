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

//! A **scheduled** process must be able to complete an **io-device round trip** —
//! `io:getopts/1`, `io:setopts/2` and `io:scan_erl_form/4`.
//!
//! # Why these are scheduled-process probes
//!
//! `io` is an ordinary `lib/stdlib/src/io.erl` module rather than a floor one: every `io:*`
//! function here (other than the floor's `io:put_chars/1,2`) posts an `{io_request, …}` to
//! the device and blocks in a `receive` for the reply. A scheduled process must complete
//! that round trip through the ordinary `RunOutcome::Wait` path — the scheduler resumes it
//! once the device replies — so `resolve_external_call` enters the body **in-machine**
//! instead of routing it onto the synchronous, **non-pumping** `call_bif` path, where a
//! slice (`may_pump_scheduler() == false`) would fold the `receive` into the typed
//! `blocked()` capability gap and exit 3. This is also the contract A.9 `compile:file/2`
//! path: `epp:open/1`'s server calls `io:getopts/1` and `epp:parse_file/2` calls
//! `io:scan_erl_form/4`.
//!
//! # What this test does
//!
//! The fixture is compiled **at test time** with the system `erlc` and driven through
//! the real executable (`beam_rt run <fixture.beam> siodev:<f>/1 "<path>"`), so the
//! entry is a genuine **scheduled** process (the `run` action uses `run_scheduled`) —
//! the `-eval` root pumps the scheduler and would not reproduce the bug. Each probe's
//! single printed line is cross-checked against the live system `erl`, so a divergence
//! shows up as a mismatch rather than being hardcoded away. A missing `erl`/`erlc` is a
//! **loud** skip, never a silent pass.

// The shared harness is compiled in whole, but this suite uses only its bounded runner and
// its toolchain discovery; the unused remainder is expected here — the same shape
// `tests/compile_file.rs` uses.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

// ---------------------------------------------------------------------------------------
// The fixture (compiled at test time — never committed as a `.beam`)
// ---------------------------------------------------------------------------------------

/// The fixture source. Three exports, each one io-device round trip reached from a
/// **scheduled** process through `file:open/2`'s `file_io_server`:
///
/// * `getopts/1` — a bare `io:getopts/1` (the exact repro shape);
/// * `opts/1` — `io:getopts/1` → `io:setopts/2` → `io:getopts/1` on one device;
/// * `scan/1` — the `epp:parse_file/2` read shape, `io:scan_erl_form/4`.
const SOURCE: &str = "\
-module(siodev).
-export([getopts/1, opts/1, scan/1]).

getopts(Path) ->
    {ok, F} = file:open(Path, [read]),
    Opts = io:getopts(F),
    ok = file:close(F),
    Opts.

opts(Path) ->
    {ok, F} = file:open(Path, [read]),
    Before = io:getopts(F),
    ok = io:setopts(F, [{encoding, utf8}]),
    After = io:getopts(F),
    ok = file:close(F),
    {Before, After}.

scan(Path) ->
    {ok, F} = file:open(Path, [read]),
    Form = io:scan_erl_form(F, \"\", {1, 1}, []),
    ok = file:close(F),
    Form.
";

/// The `io:getopts/1` answer both runtimes must print for a `[read]` (list-mode) device:
/// the bare expected value the objective records for the repro.
const GETOPTS: &str = "[{binary,false},{encoding,latin1}]";

/// The `opts/1` answer both runtimes must print: `getopts` before and after a
/// `{encoding, utf8}` `setopts`, exactly as the oracle renders it.
const OPTS: &str = "{[{binary,false},{encoding,latin1}],[{binary,false},{encoding,unicode}]}";

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// Prints the loud skip banner — a missing toolchain is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt scheduled io-device SKIPPED ===============");
    eprintln!("{why}: the scheduled io-device round trip cannot be reproduced.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("==================================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// The compiled fixture plus a readable file for it to open, in one scratch directory
/// (removed on drop). The directory never contains a `lists.beam`, so it is a safe single
/// `-pa` for the oracle.
struct Fixture {
    /// The scratch directory.
    dir: PathBuf,
    /// The compiled `siodev.beam`.
    beam: PathBuf,
    /// The file the fixture opens read-only (its own source — always present).
    readable: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Writes [`SOURCE`] and compiles it with `erlc` into a fresh scratch directory, or `None`
/// with a loud skip when the toolchain cannot do it.
fn build_fixture(erlc: &Path) -> Option<Fixture> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_sched_io_{}_{n}", std::process::id()));
    if let Err(error) = fs::create_dir_all(&dir) {
        skip_loudly(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let source = dir.join("siodev.erl");
    if let Err(error) = fs::write(&source, SOURCE) {
        skip_loudly(&format!("cannot write {}: {error}", source.display()));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            skip_loudly(&format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip_loudly(&format!(
            "`erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join("siodev.beam");
    if !beam.is_file() {
        skip_loudly(&format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixture {
        dir,
        beam,
        readable: source,
    })
}

// ---------------------------------------------------------------------------------------
// The bounded probes
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` under the shared bounded runner (`/dev/null` stdin, killed at
/// [`support::PROBE_TIMEOUT`]), failing loudly when the probe could not be run at all.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!("`{}` {args:?} could not be run to completion", program.display())
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// The **Erlang string literal** for `path`, in the double-quoted form both the `run` CLI
/// (an `ArgTerm::Str`) and the oracle's `-eval` accept. The temporary names this suite
/// builds are ASCII, which Rust's `Debug` and Erlang's string syntax quote identically.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII temporary path"))
}

/// `beam_rt run <beam> siodev:<function>/1 "<path>"` — the **scheduled** process route.
fn beam_rt_run(beam: &Path, function: &str, path: &Path) -> Run {
    let spec = format!("siodev:{function}/1");
    let beam = beam.to_str().expect("an ASCII fixture path");
    let path = literal(path);
    run(
        Path::new(BINARY),
        // `-noshell`: the boot-script drive now prints the OTP banner to stdout on a bare
        // invocation (the oracle's `erl` does the same), so suppress it to keep stdout
        // byte-identical to the oracle probe's.
        &["-noshell", "run", beam, &spec, &path],
    )
}

/// The oracle's answer for `siodev:<function>(<path>)`, rendered with `~w` so the line
/// matches the `run` action's own printer (only `~p` line-wraps).
fn oracle_line(oracle: &Path, dir: &Path, function: &str, path: &Path) -> String {
    let expr = format!(
        "io:format(\"~w~n\",[siodev:{function}({})]), halt(0).",
        literal(path)
    );
    support::oracle_eval(oracle, Some(dir), &expr)
        .unwrap_or_else(|| panic!("the live oracle could not answer siodev:{function}/1"))
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Asserts the scheduled run of `siodev:<function>/1` succeeded, printed exactly
/// `expected`, and (with the oracle present) printed what the live system `erl` prints.
///
/// `expected` is the pinned value (so the probe still discriminates with no toolchain);
/// the oracle comparison then proves the pinned value **is** the reference answer.
fn assert_scheduled_agrees(fixture: &Fixture, oracle: &Path, function: &str, expected: &str) {
    let ours = beam_rt_run(&fixture.beam, function, &fixture.readable);
    assert_eq!(
        ours.code, 0,
        "siodev:{function}/1 on a scheduled process exited {}: {}",
        ours.code, ours.err
    );
    assert!(
        !ours.err.contains("unsupported"),
        "a scheduled io-device round trip is not a capability gap: {}",
        ours.err
    );
    assert_eq!(ours.out, expected, "siodev:{function}/1's answer");
    assert_eq!(
        oracle_line(oracle, &fixture.dir, function, &fixture.readable),
        expected,
        "the pinned value must be the live oracle's"
    );
}

// ---------------------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------------------

/// A scheduled process completes an io-device round trip and answers the oracle.
///
/// Each probe enters the callee **in-machine**, so its io-device `receive` suspends the
/// process through `RunOutcome::Wait` and the scheduler resumes it when the device replies,
/// instead of folding the call into the typed `blocked()` gap on the non-pumping synchronous
/// path.
#[test]
fn a_scheduled_process_completes_an_io_device_round_trip() {
    let Some(oracle) = support::require_oracle("scheduled io-device") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixture) = build_fixture(&erlc) else {
        return;
    };

    // The exact repro shape: a bare `io:getopts/1` round trip.
    assert_scheduled_agrees(&fixture, &oracle, "getopts", GETOPTS);
    // `getopts` → `setopts` → `getopts`: two more round trips on the same device.
    assert_scheduled_agrees(&fixture, &oracle, "opts", OPTS);
    // The `epp:parse_file/2` read shape (`io:scan_erl_form/4`): the device runs the real
    // `erl_scan` and the answer is a token list, so it is compared to the oracle only.
    let scan = beam_rt_run(&fixture.beam, "scan", &fixture.readable);
    assert_eq!(
        scan.code, 0,
        "siodev:scan/1 on a scheduled process exited {}: {}",
        scan.code, scan.err
    );
    assert!(
        scan.out.starts_with("{ok,[{'-',{1,1}}"),
        "the first form scans to `{{ok, Tokens, EndPos}}`, got {}",
        scan.out
    );
    assert_eq!(
        oracle_line(&oracle, &fixture.dir, "scan", &fixture.readable),
        scan.out,
        "siodev:scan/1 must answer the live oracle's line"
    );
}
