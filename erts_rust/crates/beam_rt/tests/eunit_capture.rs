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

//! **`eunit` under this runtime**: a test's `io:format` output must be *captured*, and a failing
//! test must report that output with its failure.
//!
//! # The regression these two suites exist for
//!
//! `eunit` never lets a test write to the run's own stream. `eunit_proc` gives every test process a
//! **capture** group leader (`lib/eunit/src/eunit_proc.erl`'s `new_group_leader/1`) and keeps the
//! bytes for the failure report; a test that passes leaves no trace at all. That only works if an
//! `io` output call really reaches the caller's group leader through the
//! `{io_request, …}`/`{io_reply, …}` protocol (`src/context/io_route.rs`).
//!
//! When the floor served those keys itself, straight onto the runtime's output stream, every
//! test's `io:format` landed in the transcript: running `eunit` over `jsone` printed a spurious
//! pretty-printed block before the summary. [`a_passing_run_prints_only_eunits_summary`] is that
//! regression, and [`a_failing_test_reports_its_captured_output`] is the other half of the same
//! contract — capture exists so the output can be *reported* with the failure, so a runtime that
//! silently swallowed it would pass the first suite and still be wrong.
//!
//! # What is compared, and what is deliberately not
//!
//! Both fixtures ([`PASSING_ERL`], [`FAILING_ERL`]) are compiled at test time by the system `erlc`
//! and run through the built `beam_rt` on the `-eval` route (`-pa <scratch> -eval
//! 'R = eunit:test([M]), io:format("RESULT=~p~n",[R]).'`, the shape the crate's external suites
//! use). The passing run's pins are **parsed**, never a verbatim transcript: the transcript must be
//! exactly eunit's own summary line plus this suite's `RESULT` line, and the summary's count is
//! read back from the text.
//!
//! The failing run's pins are **structural**: eunit's failure report, the raised reason, and the
//! captured-output block with both of the test's markers in it. The report's stacktrace is *not*
//! compared byte-for-byte against the oracle — the frame list this runtime keeps is longer than
//! OTP's (a documented residual of the raise's stacktrace, not of the capture path), so the oracle
//! is cross-checked for the same structural markers and never for its bytes.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass.

#[allow(dead_code)]
mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "eunit capture";

/// The marker the **passing** fixture's tests print. It must never appear in the transcript.
const PASS_MARKER: &str = "EUNIT-NOISE";

/// The markers the **failing** fixture's test prints, and the reason it raises.
///
/// Two markers, written with two different calls (`io:format/2` and `io:put_chars/1`), so the
/// captured block proves both reached the device rather than one of them happening to be inlined.
const FAIL_MARKER: &str = "FAIL-CAPTURE-MARKER";
const FAIL_TAIL: &str = "FAIL-CAPTURE-TAIL";
const FAIL_REASON: &str = "deliberate_failure";

/// The passing fixture: three `_test` functions, two of them noisy.
///
/// Three is the smallest count whose eunit summary is the `All N tests passed.` form
/// (`lib/eunit/src/eunit_tty.erl`'s `terminate/2` has separate one- and two-test lines), so the
/// pinned line is the one the external suites (`jsone`, `bbmustache`) print too.
const PASSING_ERL: &str = r#"
-module(eunok).
-export([noise_one_test/0, noise_two_test/0, quiet_test/0]).

noise_one_test() ->
    io:format("EUNIT-NOISE-one~n"),
    io:put_chars("EUNIT-NOISE-two"),
    ok.

noise_two_test() ->
    io:format("EUNIT-NOISE-three~n"),
    ok.

quiet_test() ->
    ok.
"#;

/// The failing fixture: one noisy failing test and one quiet passing one, so the report has to
/// name the failure, carry the captured output, and still count the pass.
const FAILING_ERL: &str = r#"
-module(eunfail).
-export([noisy_fail_test/0, quiet_pass_test/0]).

noisy_fail_test() ->
    io:format("FAIL-CAPTURE-MARKER~n"),
    io:put_chars("FAIL-CAPTURE-TAIL"),
    erlang:error(deliberate_failure).

quiet_pass_test() ->
    ok.
"#;

// ---------------------------------------------------------------------------------------
// The two sides of one run
// ---------------------------------------------------------------------------------------

/// The `-eval` expression both runtimes are given for `module`.
///
/// `eunit:test/1` answers `ok` or `error`, and the expression prints it, so the run's own result
/// is a separate line the suite can pin without depending on eunit's phrasing.
fn eval_expr(module: &str) -> String {
    format!("R = eunit:test([{module}]), io:format(\"RESULT=~p~n\",[R]).")
}

/// Runs `module`'s eunit suite through the built `beam_rt` on the `-eval` route.
fn ours(fixture: &support::ErlangFixture, module: &str) -> support::Observed {
    let dir = fixture.dir().to_str().expect("an ASCII scratch path");
    let expr = eval_expr(module);
    let observed = support::run_runtime(&["-noshell", "-pa", dir, "-eval", &expr])
        .unwrap_or_else(|| panic!("the `eunit:test([{module}])` run could not complete"));
    assert_eq!(
        observed.code, 0,
        "the `-eval` run exited {}: {}",
        observed.code, observed.stderr
    );
    observed
}

/// The live oracle's stdout for the same expression, or `None` after a loud skip.
fn oracle(fixture: &support::ErlangFixture, module: &str) -> Option<String> {
    let Some(oracle) = support::resolve_oracle() else {
        support::skip_loudly(SUITE, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
        return None;
    };
    let expr = format!("{}, halt(0).", eval_expr(module));
    support::oracle_eval(&oracle, Some(fixture.dir()), &expr)
}

/// Parses eunit's passing summary — `  All N tests passed.` — and answers `N`.
///
/// The count is read back rather than pinned as text so the suite states what it means: eunit ran
/// and reported *that many* passing tests.
fn passed_count(line: &str) -> Option<usize> {
    let rest = line.trim().strip_prefix("All ")?.strip_suffix(" tests passed.")?;
    rest.parse().ok()
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// A passing run prints **only** eunit's summary — a test's `io:format` output never leaks.
///
/// The assertion is `lines == [summary, RESULT]` plus a parsed count, i.e. "nothing else was
/// written at all": any leaked byte adds a line (or joins one) and fails. The oracle must agree on
/// the summary line, which proves the pinned text is eunit's and not this runtime's invention.
#[test]
fn a_passing_run_prints_only_eunits_summary() {
    let Some(fixture) = support::ErlangFixture::compile(SUITE, "eunit_pass", "eunok", PASSING_ERL)
    else {
        return;
    };
    let observed = ours(&fixture, "eunok");
    let lines: Vec<&str> = observed.stdout.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "a passing run prints eunit's summary and this suite's result line and nothing else:\n{}",
        observed.stdout
    );
    assert_eq!(
        passed_count(lines[0]),
        Some(3),
        "the summary must report eunit's own passing count, got {:?}",
        lines[0]
    );
    assert_eq!(lines[1], "RESULT=ok", "eunit:test/1 answers `ok`");
    for marker in [PASS_MARKER, "NOISE-one", "NOISE-two", "NOISE-three"] {
        assert!(
            !observed.stdout.contains(marker),
            "a passing test's output must be captured, not leaked ({marker}):\n{}",
            observed.stdout
        );
        assert!(
            !observed.stderr.contains(marker),
            "a passing test's output must not reach stderr either ({marker}):\n{}",
            observed.stderr
        );
    }
    // The control that keeps the two assertions above from being vacuous: the same test
    // function, called from a process whose leader **is** the runtime's own output stream, prints
    // its marker. So the silence in the eunit transcript is eunit's capture leader at work, not a
    // fixture that never writes.
    let dir = fixture.dir().to_str().expect("an ASCII scratch path");
    let direct = support::run_runtime(&[
        "-noshell",
        "-pa",
        dir,
        "-eval",
        "eunok:noise_one_test(), io:format(\"RESULT=ok~n\",[]).",
    ])
    .expect("the control probe completes");
    assert_eq!(direct.code, 0, "the control probe exits 0: {}", direct.stderr);
    assert!(
        direct.stdout.contains(PASS_MARKER),
        "the fixture's noise is real when the leader is the stream:\n{}",
        direct.stdout
    );
    if let Some(expected) = oracle(&fixture, "eunok") {
        let oracle_lines: Vec<&str> = expected.lines().collect();
        assert_eq!(
            oracle_lines.first().copied().and_then(passed_count),
            Some(3),
            "the oracle must report the same count, got:\n{expected}"
        );
        assert_eq!(
            lines[0],
            oracle_lines[0],
            "the summary line must be eunit's own"
        );
        assert_eq!(lines[1], oracle_lines[1], "and this suite's result line");
    }
}

/// A failing run reports the failure **and** the test's captured output.
///
/// The captured block is the whole point of the capture leader: eunit prints it under its
/// `output:` heading, in the same `~p` rendering the device collected. Both markers must be
/// present, the failure must be named with the test's own function and the raised reason, and the
/// counts must match (`Failed: 1. … Passed: 1.`). The stacktrace is asserted **structurally**
/// only — `in function eunfail:noisy_fail_test/0` — because this runtime keeps more frames than
/// OTP does (a documented residual of the raise's stacktrace).
#[test]
fn a_failing_test_reports_its_captured_output() {
    let Some(fixture) =
        support::ErlangFixture::compile(SUITE, "eunit_fail", "eunfail", FAILING_ERL)
    else {
        return;
    };
    let observed = ours(&fixture, "eunfail");
    let stdout = &observed.stdout;
    let last = stdout.lines().last().unwrap_or_default();
    let structural = [
        // eunit's own header for a failed test, naming the fixture's function…
        "eunfail: noisy_fail_test...*failed*",
        // …the test's frame (the one stacktrace line this suite pins)…
        "in function eunfail:noisy_fail_test/0",
        // …and the captured-output block, with both of the test's writes in it.
        "output:",
        FAIL_MARKER,
        FAIL_TAIL,
        // The run's counts: one failure, no skips, one pass.
        "Failed: 1.  Skipped: 0.  Passed: 1.",
    ];
    for needle in structural {
        assert!(
            stdout.contains(needle),
            "the failure report must contain {needle:?}, got:\n{stdout}"
        );
    }
    assert_eq!(last, "RESULT=error", "eunit:test/1 answers `error`");
    let captured_from = stdout
        .find("output:")
        .unwrap_or_else(|| panic!("eunit must print the captured output:\n{stdout}"));
    let captured = &stdout[captured_from..];
    for needle in [FAIL_MARKER, FAIL_TAIL] {
        assert!(
            captured.contains(needle),
            "the captured-output block must carry {needle:?}, got:\n{captured}"
        );
    }
    // The raise itself is reported above the captured block, as eunit renders it.
    let reason_line = format!("**error:{FAIL_REASON}");
    assert!(
        stdout.contains(&reason_line),
        "the report must name the raised reason ({reason_line:?}):\n{stdout}"
    );
    if let Some(expected) = oracle(&fixture, "eunfail") {
        for needle in structural {
            assert!(
                expected.contains(needle),
                "the oracle's report must carry the same marker {needle:?} — the fixture's shape \
                 is wrong if it does not:\n{expected}"
            );
        }
        assert_eq!(
            expected.lines().last().unwrap_or_default(),
            last,
            "the two runtimes must agree on the run's own result line"
        );
    } else {
        support::skip_loudly(SUITE, "the live oracle could not run the failing fixture");
    }
}
