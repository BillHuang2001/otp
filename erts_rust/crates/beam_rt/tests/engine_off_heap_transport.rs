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

//! **An engine-built OFF-HEAP (refc) binary survives the transport out of a scheduled
//! process** — the end-to-end regression coverage for the runner's two copy sites.
//!
//! # The bug this suite pins
//!
//! A workload that runs as a **scheduled** process (`beam_rt run <file> <Mod>:<F>/<A>`) has
//! its result and its exit reason copied onto the runtime's pointer-stable `eval_heap`
//! before the process's own heap dies with it. That copy used the *simple* form of the
//! `beam_message` copy engine, whose **source off-heap table is empty** — so
//! `copy_refc_binary` could not resolve the payload handle of a refc binary the *engine*
//! built and refused the whole term with `BeamError::BadArg`, which the CLI reported as
//! `beam_rt: badarg` (exit `1`).
//!
//! The failing shape is not exotic: `base64:encode/1` accumulates through
//! `bs_init_writable`, which the engine builds as the frozen 8-word `[BinRef][ErlSubBits]`
//! pair over an **off-heap** payload (`beam_engine/src/families/bitstrings/raw.rs`) — even a
//! **0-capacity writable accumulator** (an empty result) is off-heap. So
//! `base64:encode(<<>>)` — the emptiest possible binary — was the smallest reproducer.
//!
//! The fix resolves the payload through the *machine's* handles
//! (`Runtime::stabilize_term_with` / `Runtime::try_stabilize_escaped_with`, fed by
//! `Machine::refc_binaries()` from the runner's `Returned` and raise arms) and hands the
//! runtime's `eval_off_heap` destination table the ownership of the copied payload.
//!
//! # What each half pins
//!
//! 1. [`the_regression_repro_returns_an_empty_binary`] — the exact reproducer module
//!    (`e/0` = `base64:encode(<<>>)`, `f/1` = `base64:encode(binary:copy(<<A>>, 0))`): both
//!    must print the **empty binary** `<<>>`, not `badarg`.
//! 2. [`engine_built_off_heap_binaries_survive_the_transport_copy`] — a >64-byte engine-built
//!    binary, a several-hundred-byte one, and one returned straight from an accumulating
//!    construction, each byte-compared with the live oracle.
//! 3. [`on_heap_results_still_transport_out_of_a_scheduled_process`] — the **enabling
//!    structure**: an on-heap result (a small binary, a plain heap term), an
//!    **engine-built on-heap** binary (a `bs_create_bin` result under the on-heap limit) and
//!    on-heap **self-sends** must keep working. A regression of the fix that broke the
//!    ordinary copy would show up here.
//! 4. [`an_engine_built_off_heap_exit_reason_renders_its_real_bytes`] — the *second* fixed
//!    arm: a process that raises/`exit`s with a >64-byte engine-built binary must render the
//!    **real bytes** in its exit reason, never a placeholder or a dangling term.
//!
//! # What is deliberately NOT pinned here
//!
//! The **message** path is still short of the same plumbing: an engine-built **off-heap**
//! binary sent between processes (`self() ! <<0:(1200)/integer>>` to self) still yields
//! `beam_rt: badarg`, because the engine does not yet hand its off-heap handles to the
//! mailbox copy (an additive `beam_engine` context seam; recorded as a Known Issue in
//! `src/context/CONTEXT.md`). Pinning that as "expected" would enshrine a non-oracle
//! answer, so no test here asserts it.
//!
//! # Conventions
//!
//! The fixture is compiled **at test time** with the system `erlc` into a scratch directory
//! (never committed — the source of truth is the `.erl` below), driven through the **built
//! binary** (`beam_rt run`, the same executable the reproducer used) and cross-checked against
//! the live system `erl` running the **same** call. A missing `erl`/`erlc` is a **loud** skip,
//! never a silent pass (`tests/support/mod.rs`). The scratch directory holds only the fixture
//! module, so it is safe to hand to the oracle as a single `-pa` (unlike this crate's
//! `tests/fixtures/`, which contains a `lists.beam`).
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod support;

/// The fixture's module name.
const MODULE: &str = "offheap";

/// The suite's name in the loud-skip banner.
const SUITE: &str = "engine_off_heap_transport";

/// The fixture source (compiled at test time — never committed as a `.beam`).
///
/// Every export is a distinct transport shape; the doc comments are the `.erl` file's own
/// record of what each one proves.
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/engine_off_heap_transport.rs` (never committed).
-module(offheap).
-export([e/0, f/1, big/0, huge/0, copied/0, tiny/0, mkbin/1, list/0, selfsend/0,
         selfbin/0, boom/0, rtboom/0]).

%% --- The exact regression reproducer ------------------------------------------------
%% `base64:encode/1` accumulates through `bs_init_writable`, which the engine builds as a
%% refc-binary pair over an OFF-HEAP payload -- even at 0 capacity, so the empty result is
%% the smallest case that used to die in the transport copy with `badarg`.
e()   -> base64:encode(<<>>).
f(A) -> base64:encode(binary:copy(<<A>>, 0)).

%% --- Engine-built off-heap binaries --------------------------------------------------
%% A 16-byte result (the recorded regression example) ...
big()    -> base64:encode(binary:copy(<<1,2,3,4,5,6,7,8,9,10>>, 1)).
%% ... a 400-byte one, again through the `bs_init_writable` accumulator (300 bytes in,
%% 100 whole 3-byte blocks => `"AQID"` repeated 100 times out) ...
huge()   -> base64:encode(binary:copy(<<1,2,3>>, 100)).
%% ... and a 300-byte one returned straight from the accumulating construction
%% (a `binary:copy/2` result, past `ERL_ONHEAP_BINARY_LIMIT`).
copied() -> binary:copy(<<7,8,9>>, 100).

%% --- On-heap controls (the enabling structure) ---------------------------------------
%% A small binary from `binary:copy/2` ...
tiny()    -> binary:copy(<<1,2>>, 1).
%% ... an ENGINE-BUILT on-heap binary (`bs_create_bin`, 2 bytes, under the on-heap limit) ...
mkbin(A)  -> <<A, 2>>.
%% ... a plain heap term ...
list()    -> lists:seq(1, 5).
%% ... and on-heap self-sends through a compiled `receive`.
selfsend() -> self() ! 42, receive M -> M end.
selfbin()  -> self() ! <<1,2>>, receive B -> B end.

%% --- The exit-reason arm -------------------------------------------------------------
%% `<<0:(1200)/integer>>` is 150 bytes, so the engine's `bs_create_bin` builds it off-heap;
%% it is NOT a literal (a `literals` chunk probe shows only the four small constants above).
boom()   -> exit(<<0:(1200)/integer>>).
%% The same shape reached through the writable accumulator: a 200-byte exit reason.
rtboom() -> exit(base64:encode(binary:copy(<<0:(1200)/integer>>, 1))).
"#;

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
///
/// `$ERTS_ORACLE`/`$PATH` discovery is `tests/support/mod.rs`'s (filesystem only — a probe
/// must never *run* `erl` to find out whether it exists).
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// The live oracle and a compiled fixture, or `None` after a **loud** skip.
fn setup(label: &str) -> Option<(PathBuf, Fixture)> {
    let oracle = support::require_oracle(SUITE)?;
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(
            SUITE,
            "no `erlc` available (and none beside $ERTS_ORACLE)",
        );
        return None;
    };
    let fixture = build_fixture(&erlc, label)?;
    Some((oracle, fixture))
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding `offheap.erl` and `offheap.beam`.
    dir: PathBuf,
    /// The compiled `offheap.beam`.
    beam: PathBuf,
}

impl Fixture {
    /// The compiled image as a `str`, for the CLI's argv.
    fn beam_str(&self) -> &str {
        self.beam
            .to_str()
            .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", self.beam.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test, yet a
        // cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Writes [`FIXTURE_ERL`] into a fresh scratch directory and compiles it there, or `None`
/// when it cannot be compiled (the caller then returns early — **loudly**).
///
/// `erlc` runs **with the scratch directory as cwd** and the bare argument, exactly as
/// `tests/spawn_from_slice.rs` does, so the module is self-contained and the scratch dir is a
/// safe single `-pa` for the oracle.
fn build_fixture(erlc: &Path, label: &str) -> Option<Fixture> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_offheap_{label}_{}_{n}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join("offheap.erl"), FIXTURE_ERL))
    {
        support::skip_loudly(SUITE, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg("offheap.erl")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(SUITE, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            SUITE,
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let beam = dir.join("offheap.beam");
    if !beam.is_file() {
        support::skip_loudly(SUITE, &format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixture { dir, beam })
}

// ---------------------------------------------------------------------------------------
// The bounded probe
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed and
/// reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite.
///
/// `beam_rt run` is itself bounded by a wall-clock deadline and every fixture entry either
/// returns or raises, so the polling guard is the belt to that suspenders (the same local
/// runner `tests/spawn_from_slice.rs` keeps, for the same reason).
fn run_bounded(program: &Path, args: &[&str]) -> Run {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn `{}`: {error}", program.display()));
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    let output: Output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child
                .wait_with_output()
                .unwrap_or_else(|error| panic!("cannot collect the probe's output: {error}")),
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "`{}` outlived the {}s probe bound",
                    program.display(),
                    support::PROBE_TIMEOUT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot poll `{}`: {error}", program.display()),
        }
    };
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// The shared assertions
// ---------------------------------------------------------------------------------------

/// The `~w` rendering of a byte slice, i.e. exactly how Erlang prints a binary: `<<1,2,3>>`.
///
/// Computing the expected line here (rather than pasting the printer's output) keeps the
/// expectation an *independent* statement of the bytes the workload must return; the live
/// oracle is the second, independent witness.
fn w_binary(bytes: &[u8]) -> String {
    let mut out = String::from("<<");
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&byte.to_string());
    }
    out.push_str(">>");
    out
}

/// The Erlang call both runtimes are given: `offheap:<function>(<args…>)`.
fn call(function: &str, args: &[&str]) -> String {
    format!("{MODULE}:{function}({})", args.join(","))
}

/// Builds the `beam_rt run <beam> <Module>:<Function>/<Arity> [args…]` argv.
fn run_argv<'a>(beam: &'a str, spec: &'a str, args: &[&'a str]) -> Vec<&'a str> {
    let mut argv = vec!["run", beam, spec];
    argv.extend_from_slice(args);
    argv
}

/// Asserts `run offheap:<function>/<arity>` prints exactly `expected` with exit code `0` and
/// an empty stderr, **and** that the live oracle prints the same line for the same call.
fn assert_return_matches_the_oracle(
    oracle: &Path,
    fixture: &Fixture,
    label: &str,
    function: &str,
    args: &[&str],
    expected: &str,
) {
    let spec = format!("{MODULE}:{function}/{}", args.len());
    let argv = run_argv(fixture.beam_str(), &spec, args);
    let ours = run_bounded(Path::new(env!("CARGO_BIN_EXE_beam_rt")), &argv);
    assert_eq!(
        ours.code, 0,
        "{label}: `beam_rt run {} {spec} {args:?}` exited {}: {}",
        fixture.beam_str(),
        ours.code,
        ours.err
    );
    assert!(
        !ours.err.contains("unsupported"),
        "{label}: not a capability gap: {}",
        ours.err
    );
    assert!(ours.err.is_empty(), "{label}: stderr: {}", ours.err);
    assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

    let expr = format!(
        "io:format(\"~w~n\", [{}]), halt(0).",
        call(function, args)
    );
    let theirs = support::oracle_eval(oracle, Some(&fixture.dir), &expr)
        .unwrap_or_else(|| panic!("{label}: the live oracle could not answer"));
    assert_eq!(
        theirs.lines().next().unwrap_or_default(),
        expected,
        "{label}: the expected line must be the live oracle's"
    );
}

/// Asserts `run offheap:<function>/<arity>` **raises** with a reason rendering exactly
/// `expected_reason` (exit code `1`, empty stdout), **and** that the live oracle's caught
/// reason renders the same bytes.
///
/// Only the *reason term* is compared with the oracle: the oracle's uncaught `-eval`
/// diagnostic for an `exit/1` reason is the emulator's own `init`-termination line, a
/// different shape from this runtime's `beam_rt: <error>` diagnostic.
fn assert_exit_reason_matches_the_oracle(
    oracle: &Path,
    fixture: &Fixture,
    label: &str,
    function: &str,
    args: &[&str],
    reason_bytes: &[u8],
) {
    let expected = w_binary(reason_bytes);
    let spec = format!("{MODULE}:{function}/{}", args.len());
    let argv = run_argv(fixture.beam_str(), &spec, args);
    let ours = run_bounded(Path::new(env!("CARGO_BIN_EXE_beam_rt")), &argv);
    assert_eq!(
        ours.code, 1,
        "{label}: a raise is a runtime error (exit 1): {}",
        ours.err
    );
    assert!(ours.out.is_empty(), "{label}: stdout: {}", ours.out);
    assert_eq!(
        ours.err,
        format!("beam_rt: error {expected}"),
        "{label}: the reason must render its real bytes"
    );

    // The oracle's witness is the *reason term*: `~w` of what the same call raises.
    let expr = format!(
        "try {} catch _C:R -> io:format(\"~w~n\", [R]) end, halt(0).",
        call(function, args)
    );
    let theirs = support::oracle_eval(oracle, Some(&fixture.dir), &expr)
        .unwrap_or_else(|| panic!("{label}: the live oracle could not answer"));
    assert_eq!(
        theirs.lines().next().unwrap_or_default(),
        expected,
        "{label}: the oracle renders the same reason bytes"
    );
}

// ---------------------------------------------------------------------------------------
// 1. The exact regression reproducer
// ---------------------------------------------------------------------------------------

/// **The smallest possible reproducer of the transport bug.**
///
/// `base64:encode/1` accumulates through `bs_init_writable`, which the engine builds as a
/// refc-binary pair over an **off-heap** payload — at *any* capacity, including the 0-capacity
/// accumulator that produces an empty result. Before the fix both entries below died in the
/// runner's transport copy with `BeamError::BadArg` (`beam_rt: badarg`, exit `1`); both must
/// now print the empty binary `<<>>`, byte-identical to the oracle.
#[test]
fn the_regression_repro_returns_an_empty_binary() {
    let Some((oracle, fixture)) = setup("repro") else {
        return;
    };
    let empty = w_binary(&[]);
    assert_eq!(empty, "<<>>", "an empty binary's `~w` form");

    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "base64:encode(<<>>) (a 0-capacity writable accumulator)",
        "e",
        &[],
        &empty,
    );
    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "base64:encode(binary:copy(<<A>>, 0)) (the exact regression repro)",
        "f",
        &["1"],
        &empty,
    );
}

// ---------------------------------------------------------------------------------------
// 2. Off-heap results of every size
// ---------------------------------------------------------------------------------------

/// **A refc binary the engine built must survive the copy out of the process at every size.**
///
/// All three results are past `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` (64 bytes) except the
/// first, which is the recorded regression example; the 400-byte one is the strongest case (a
/// truncated, elided or dangling copy would not reach the printer at that length).
#[test]
fn engine_built_off_heap_binaries_survive_the_transport_copy() {
    let Some((oracle, fixture)) = setup("off_heap") else {
        return;
    };

    // The recorded regression example: 10 bytes in, 16 bytes out.
    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "base64:encode(binary:copy(<<1..10>>, 1))",
        "big",
        &[],
        &w_binary(b"AQIDBAUGBwgJCg=="),
    );
    // A several-hundred-byte (400-byte) result: 100 whole 3-byte blocks, so the base64 text
    // is `AQID` repeated exactly 100 times.
    let huge: Vec<u8> = b"AQID".repeat(100);
    assert_eq!(huge.len(), 400);
    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "base64:encode(binary:copy(<<1,2,3>>, 100))",
        "huge",
        &[],
        &w_binary(&huge),
    );
    // A several-hundred-byte (300-byte) result returned straight from the accumulating
    // construction, past the on-heap limit.
    let copied: Vec<u8> = [7u8, 8, 9].repeat(100);
    assert_eq!(copied.len(), 300);
    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "binary:copy(<<7,8,9>>, 100)",
        "copied",
        &[],
        &w_binary(&copied),
    );
}

// ---------------------------------------------------------------------------------------
// 3. The on-heap controls (the enabling structure)
// ---------------------------------------------------------------------------------------

/// **The ordinary paths keep working** — the guard that the fix did not trade one shape for
/// another.
///
/// A small (on-heap) `binary:copy/2` result, an **engine-built on-heap** binary (a
/// `bs_create_bin` result under the on-heap limit), a plain heap term, and on-heap
/// **self-sends** through a compiled `receive` must all still transport out of the process.
#[test]
fn on_heap_results_still_transport_out_of_a_scheduled_process() {
    let Some((oracle, fixture)) = setup("on_heap") else {
        return;
    };

    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "binary:copy(<<1,2>>, 1) (a small on-heap binary)",
        "tiny",
        &[],
        &w_binary(&[1, 2]),
    );
    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "<<A, 2>> (an engine-built on-heap binary)",
        "mkbin",
        &["7"],
        &w_binary(&[7, 2]),
    );
    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "lists:seq(1, 5) (a plain heap term)",
        "list",
        &[],
        "[1,2,3,4,5]",
    );
    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "self() ! 42, receive M -> M end",
        "selfsend",
        &[],
        "42",
    );
    assert_return_matches_the_oracle(
        &oracle,
        &fixture,
        "self() ! <<1,2>>, receive B -> B end",
        "selfbin",
        &[],
        &w_binary(&[1, 2]),
    );
}

// ---------------------------------------------------------------------------------------
// 4. The exit-reason arm
// ---------------------------------------------------------------------------------------

/// **The second fixed copy site: a raised off-heap binary renders its real bytes.**
///
/// `exit(<<0:(1200)/integer>>)` raises with a 150-byte binary the engine built with
/// `bs_create_bin` (no `literals` chunk entry — proved by a `beam_lib:chunks/2` probe), so its
/// payload is off-heap; the second entry reaches the same shape through `base64:encode`'s
/// writable accumulator. Both reasons must render their real bytes — a dangling or
/// placeholder term would print something else entirely.
#[test]
fn an_engine_built_off_heap_exit_reason_renders_its_real_bytes() {
    let Some((oracle, fixture)) = setup("exit_reason") else {
        return;
    };

    assert_exit_reason_matches_the_oracle(
        &oracle,
        &fixture,
        "exit(<<0:(1200)/integer>>)",
        "boom",
        &[],
        &[0u8; 150],
    );
    assert_exit_reason_matches_the_oracle(
        &oracle,
        &fixture,
        "exit(base64:encode(binary:copy(<<0:(1200)/integer>>, 1)))",
        "rtboom",
        &[],
        &[65u8; 200],
    );
}
