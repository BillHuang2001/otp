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
//! **A retained `-eval` value must survive the statement window's reset** (A.1/A.6).
//!
//! `Runtime::eval_heap` is the pointer-stable region a `-eval` run's container results are copied
//! onto (`src/context/pool.rs::move_result_into_runtime_store`) and
//! `src/eval/scratch.rs`'s statement window rewinds its dead prefix at the boundary **before**
//! every top-level statement. Between the two, the value one statement builds and a later
//! statement reads has to be unreachable-from-nowhere for the reset to be sound; a window that
//! reclaimed it would show up here as a **wrong number**, which is exactly the failure mode that
//! must never ship.
//!
//! Each case therefore keeps a `-eval` value alive across twenty throwaway builds — a shape
//! chosen because `discarded_builds` is the reducible one (`PERF.md`): the twenty statements
//! between the holder and the reduce are each worth a reset, so a false recycle has twenty
//! chances to happen.
//!
//! The three holder families the descent can enumerate are covered by a fourth that it cannot:
//!
//! 1. a `persistent_term` value — a **runtime-lifetime store**, whose writer latches the window
//!    (`Runtime::stabilize_term` → `scratch::pin`) instead of letting the descent prove it dead;
//! 2. an **environment binding** — a root of the descent;
//! 3. a **mailbox message** — an enumerated root family.
//!
//! Every case is compared with the system Erlang oracle by **trimmed stdout equality plus the
//! exit code**, with the rendering pinned as a golden too, so a regression fails even where the
//! toolchain is absent. A missing toolchain is a **loud skip**, never a silent pass; the probes
//! print only an integer the two runtimes agree on, never a version string or any other
//! own-state figure.
#[allow(dead_code)]
mod support;

use std::process::Command;

/// The width of the retained value's elements: a list of this size copies thousands of words,
/// so it is unmistakably *the* window any reset would be reclaiming.
const ELEMENTS: u32 = 2_000;

/// How many throwaway statements each case interposes between the holder and the reduce.
///
/// Twenty is well past the one candidate the sharpest shape needs (the boundary immediately
/// after the holder's statement) and still keeps the oracle probe fast: every one of them is a
/// `lists:seq/2` the window resets.
const BUILDS: usize = 20;

/// `BUILDS` statements that each build and discard a large term, comma-terminated, so a caller
/// appends its own final statement.
///
/// This is the reducible shape `src/eval/scratch.rs` exists for: the language discards every
/// statement's value but the last, so each boundary may reclaim the previous statement's copy.
fn discarded_builds() -> String {
    let statement = format!("_ = lists:seq(1,{ELEMENTS}), ");
    let mut expr = String::with_capacity(BUILDS * statement.len());
    for _ in 0..BUILDS {
        expr.push_str(&statement);
    }
    expr
}

/// Runs `beam_rt -noshell -eval <expr>` and returns `(trimmed stdout, exit code)`.
///
/// The CLI's documented `-eval` route (contract A.6) prints nothing of its own, so everything
/// observed below is what the expression itself printed.
fn beam_eval(expr: &str) -> (String, i32) {
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(["-noshell", "-eval", expr])
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt for {expr:?} failed: {error}"));
    (
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// Asserts this runtime prints `golden` for `expr`, that the value is a real answer rather than
/// the `after` clause's `timeout`, and that the system oracle prints the same bytes.
///
/// The oracle is resolved **first**: a host with no toolchain is a loud skip, and it is also the
/// host on which this runtime cannot boot at all (its system code path is resolved from the
/// filesystem, with the system `erl`'s own report as the documented last resort), so asserting
/// before the probe would turn "no toolchain" into a spurious failure.
fn assert_agrees_with_the_oracle(banner: &str, expr: &str, golden: &str) {
    let Some(oracle) = support::require_oracle(banner) else {
        return;
    };
    let (stdout, exit) = beam_eval(expr);
    assert_eq!(
        exit, 0,
        "`-eval` must succeed: a recycled store word is read as a pointer and aborts the process \
         (stdout {stdout:?})"
    );
    assert_ne!(stdout, "timeout", "the retained value never arrived");
    assert!(
        stdout.chars().all(|c| c.is_ascii_digit()) && !stdout.is_empty(),
        "the expression must answer a number, not {stdout:?}"
    );
    assert_eq!(
        stdout, golden,
        "the reduce must read the value the first statement built — a reset that reclaimed it \
         shows up as a different total"
    );
    match support::oracle_eval(&oracle, None, expr) {
        Some(expected) => assert_eq!(
            stdout, expected,
            "the retained value must reduce to the system oracle's total"
        ),
        None => eprintln!("warning: the oracle could not answer `{expr}`"),
    }
}

/// `3 * Σ 1..ELEMENTS` — the total every `{I,I,I}` case reduces to.
const TRIPLE_SUM: &str = "6003000";
/// `Σ 1..ELEMENTS` — the total the `{I,I}` case reduces to.
const PAIR_SUM: &str = "2001000";

/// A `persistent_term` value: the one holder family the descent **cannot** enumerate, so this is
/// the pin for the latch — the writer's `pin` is what keeps the twenty boundaries around it from
/// resetting the words the store names.
#[test]
fn a_persistent_term_value_survives_the_statement_window() {
    let expr = format!(
        "persistent_term:put(er_k, [{{I,I,I}} || I <- lists:seq(1,{ELEMENTS})]), \
         {builds}\
         io:format(\"~p~n\", [lists:sum([A+B+C || {{A,B,C}} <- persistent_term:get(er_k)])]).",
        builds = discarded_builds(),
    );
    assert_agrees_with_the_oracle("eval_region", &expr, TRIPLE_SUM);
}

/// An environment binding: a root of the descent, so every boundary that sees it declines and the
/// ones after it may reclaim.
#[test]
fn an_environment_binding_survives_the_statement_window() {
    let expr = format!(
        "A = [{{I,I,I}} || I <- lists:seq(1,{ELEMENTS})], \
         {builds}\
         io:format(\"~p~n\", [lists:sum([X+Y+Z || {{X,Y,Z}} <- A])]).",
        builds = discarded_builds(),
    );
    assert_agrees_with_the_oracle("eval_region", &expr, TRIPLE_SUM);
}

/// A mailbox message: an enumerated root family, and the sharpest of the three — the message is
/// copied into this process's mailbox *before* the twenty builds, so a reset that ran while that
/// copy was live would be read back by the `receive` at the end.
#[test]
fn a_mailbox_message_survives_the_statement_window() {
    let expr = format!(
        "self() ! [{{I,I}} || I <- lists:seq(1,{ELEMENTS})], \
         {builds}\
         receive L -> io:format(\"~p~n\", [lists:sum([A || {{A,_}} <- L])]) \
         after 5000 -> timeout end.",
        builds = discarded_builds(),
    );
    assert_agrees_with_the_oracle("eval_region", &expr, PAIR_SUM);
}
