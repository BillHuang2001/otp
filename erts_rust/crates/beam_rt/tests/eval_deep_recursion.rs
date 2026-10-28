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
//! **A deep self-recursive named fun on the `-eval` route** — the nesting-budget regression
//! suite.
//!
//! # The defect this pins
//!
//! The `-eval` evaluator is a recursive-descent tree walker, so a closure it creates has the
//! **native Rust stack** for a continuation: `F = fun G(0) -> done; G(N) -> G(N - 1) end,
//! F(50000)` used to enter `apply_closure` once per Erlang level. At ≈1.4 KiB of Rust stack per
//! level against the driver's 64 MiB thread (`src/main.rs::MAIN_STACK_BYTES`) the walker
//! overran it: the process died with `fatal runtime error: stack overflow, aborting` (SIGABRT,
//! exit 134) where the oracle answers `done`.
//!
//! `src/eval/nesting.rs` is the budget that makes the route memory-safe. Past
//! `WALK_APPLY_LIMIT` walked applications, a further closure application is served by the
//! **existing** compile route (`src/eval/compile.rs`): the closure's recorded source span
//! becomes a synthetic `beam_rt_eval_fun_<index>` module, so the recursion runs on the BEAM
//! process stack and answers the oracle's value. The shape no compile can turn into BEAM
//! recursion — a fun that *alternates* between a compiled module and a walked one — is the
//! typed `unsupported: eval fun application: …` capability gap (exit 3), never an abort.
//!
//! # What is asserted, and why each half is load-bearing
//!
//! * The deep recursion **reaches the oracle's value** (`done`) with exit 0. A shrink of the
//!   fixture or a "refusal only" fix would fail here, which is the point: 50 000 levels is
//!   past both the release (47 000–49 000) and dev (30 000–35 000) pre-fix cut.
//! * The process is **not signal-killed**: the exit status is a real code (`Some(0)`), so a
//!   regression to a stack overflow is reported as such rather than as a mismatched string.
//! * The alternating shape ends in the **typed** gap (exit 3, one `unsupported:` line, empty
//!   stdout) — the runtime's documented refusal, not a signal.
//! * When a system `erl` is present the same expression is run under it and the value is
//!   compared, so the pinned `done` is the oracle's answer and not this suite's invention.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
//!     --test eval_deep_recursion
//! ```
mod support;

/// The suite's banner in a loud skip.
const SUITE: &str = "eval_deep_recursion";

/// The runtime binary under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// A tail-recursive named fun, 50 000 levels deep, printing its answer.
///
/// Past the pre-fix stack cut in **both** profiles (release ≈47 000, dev ≈30 000), so the
/// value can only come from the compiled route rather than by luck.
const DEEP_TAIL: &str = "F = fun G(0) -> done; G(N) -> G(N - 1) end, \
                         io:format(\"~p~n\", [F(50000)]).";

/// A deeper still, tail-recursive call: the bound must not be a depth limit on the value.
const DEEPER_TAIL: &str = "F = fun G(0) -> done; G(N) -> G(N - 1) end, \
                            io:format(\"~p~n\", [F(100000)]).";

/// A **non-tail** recursion: each level keeps a 50 000-long list, so the answer exercises the
/// compiled module's own stack and heap, not just a counter.
const DEEP_NON_TAIL: &str = "F = fun G(0) -> []; G(N) -> [N | G(N - 1)] end, \
                             io:format(\"~p~n\", [length(F(50000))]).";

/// A fun that **alternates** between a compiled module and a walked one: `F(F, N)` climbs one
/// walker level, which compiles and calls the module, whose body applies `F` again — the shape
/// the compiled route cannot turn into pure BEAM recursion, so it must answer the typed gap.
const ALTERNATING: &str = "F = fun(G, 0) -> done; (G, N) -> G(G, N - 1) end, F(F, 10000).";

/// Runs the runtime binary with `-noshell -eval expr` and answers `(code, stdout, stderr)`.
///
/// The status is a **code**, never `None`: a child killed by a signal is exactly the pre-fix
/// failure and must be reported as a failure here, so the caller asserts on the code and this
/// helper stays total.
fn run_eval(expr: &str) -> (Option<i32>, String, String) {
    let output = support::run_bounded(
        std::path::Path::new(BINARY),
        &["-noshell", "-eval", expr],
    )
    .unwrap_or_else(|| panic!("the runtime binary did not answer within the probe bound: {BINARY}"));
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The oracle's stdout for `expr`, or `None` after a loud skip when no `erl` is installed.
fn oracle_stdout(expr: &str) -> Option<String> {
    let oracle = support::require_oracle(SUITE)?;
    let output = support::run_bounded(&oracle, &["-noshell", "-eval", expr, "-s", "init", "stop"])
        .unwrap_or_else(|| panic!("the oracle did not answer within the probe bound"));
    assert!(
        output.status.success(),
        "the oracle failed on {expr:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fails with the full transcript when a run's stdout is not the expected one.
fn assert_stdout(code: Option<i32>, stdout: &str, stderr: &str, expected: &str, what: &str) {
    assert_eq!(
        code,
        Some(0),
        "{what} must exit 0 (a signal-killed run is the stack-overflow regression)\n\
         stdout: {stdout:?}\nstderr: {stderr:?}"
    );
    assert_eq!(stdout, expected, "{what} must print the oracle's value");
}

/// The reported defect, on the binary: a 50 000-deep named-fun recursion answers, not aborts.
#[test]
fn a_deep_named_fun_recursion_answers_the_oracles_value() {
    let (code, stdout, stderr) = run_eval(DEEP_TAIL);
    assert_stdout(code, &stdout, &stderr, "done\n", "the 50 000-deep tail recursion");
    if let Some(expected) = oracle_stdout(DEEP_TAIL) {
        assert_eq!(stdout, expected, "the runtime's value must be the oracle's");
    }
}

/// The bound is not a depth limit on the *value*: a deeper call still answers.
#[test]
fn a_deeper_recursion_still_answers() {
    let (code, stdout, stderr) = run_eval(DEEPER_TAIL);
    assert_stdout(code, &stdout, &stderr, "done\n", "the 100 000-deep tail recursion");
}

/// A non-tail recursion keeps one list element per level, so the compiled module's own stack
/// and heap carry the continuation — the walker's frames are not what holds it.
#[test]
fn a_non_tail_recursion_keeps_its_frames_on_the_beam_stack() {
    let (code, stdout, stderr) = run_eval(DEEP_NON_TAIL);
    assert_stdout(
        code,
        &stdout,
        &stderr,
        "50000\n",
        "the 50 000-deep non-tail recursion",
    );
}

/// The shape no compile can serve answers the **typed** capability gap: exit 3, one
/// `unsupported:` line on stderr, and nothing on stdout — never a signal, never a panic.
#[test]
fn a_recursion_that_alternates_compiled_and_walked_answers_a_typed_gap() {
    let (code, stdout, stderr) = run_eval(ALTERNATING);
    assert_eq!(
        code,
        Some(3),
        "the alternating recursion must answer the typed gap, not exit {code:?}\n\
         stdout: {stdout:?}\nstderr: {stderr:?}"
    );
    assert_eq!(stdout, "", "a refused run prints nothing on stdout");
    assert_eq!(
        stderr.lines().count(),
        1,
        "the refusal is exactly one line: {stderr:?}"
    );
    assert!(
        stderr.starts_with("beam_rt: unsupported: eval fun application:"),
        "the refusal names the nesting budget: {stderr:?}"
    );
    // The alternating shape's *prefix* is past the walker bound by construction: `F(F, N)`
    // climbs one walker level per call and compiles at the bound.
    assert!(
        stderr.contains("recursion bound"),
        "the refusal states the bound it hit: {stderr:?}"
    );
}
