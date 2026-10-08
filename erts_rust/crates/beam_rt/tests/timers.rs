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

//! `erlang:send_after/3` + `erlang:cancel_timer/1` at the **executable** boundary.
//!
//! `src/runtime.rs`'s unit tests prove the runtime-local deadline heap end to end against
//! the real engine, mailbox and scheduler; this test proves the same capability through the
//! built `beam_rt` binary and the full `-eval` path, so the CLI exit code and the printed
//! value are pinned too.
//!
//! The runtime has no timer **thread** (`Runtime` is `!Send`): `erlang:send_after/3` puts an
//! absolute deadline on a min-heap the scheduler loop drains, and a process blocked in
//! `receive` is woken by the delivered message through the ordinary resume-on-send path.
//!
//! Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`):
//!
//! ```none
//! $ erl -noshell -eval 'io:format("~p~n",[begin erlang:send_after(5,self(),hello),
//!                                             receive X -> X end end]),halt(0).'
//! hello
//! ```
//!
//! and, for the cancel answers,
//!
//! ```none
//! $ erl -noshell -eval 'io:format("~p~n",[is_integer(
//!     erlang:cancel_timer(erlang:send_after(10000,self(),x)))]),halt(0).'
//! true
//! $ erl -noshell -eval 'io:format("~p~n",[erlang:cancel_timer(make_ref())]),halt(0).'
//! false
//! ```
use std::process::{Command, Output};

/// Runs the built `beam_rt` binary with `args`.
fn beam_rt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// The stdout of a finished run, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a finished run, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A message an `erlang:send_after/3` timer delivers is what the blocked `receive` takes:
/// the evaluator schedules the deadline, blocks on the empty mailbox, the `-eval` pump
/// advances the runtime-local heap and the delivered message becomes `X`.
///
/// Oracle: `erlang:send_after(5, self(), hello), receive X -> X end` ⇒ `hello`.
#[test]
fn eval_send_after_wakes_a_blocking_receive_and_prints_the_message() {
    let output = beam_rt(&[
        "-noshell",
        "-eval",
        "io:format(\"~p~n\",[begin erlang:send_after(5, self(), hello), \
         receive X -> X end end]).",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "hello\n");
    assert!(output.stderr.is_empty(), "stderr: {}", stderr(&output));
}

/// A pending timer's reference is cancellable and `cancel_timer/1` answers the remaining
/// time as an integer; a reference that is not a timer reference answers the oracle's
/// `false`.
///
/// Oracle: `is_integer(cancel_timer(send_after(10000, self(), x)))` ⇒ `true`;
/// `cancel_timer(make_ref())` ⇒ `false`.
#[test]
fn eval_cancel_timer_answers_a_remaining_time_then_false() {
    let output = beam_rt(&[
        "-noshell",
        "-eval",
        "io:format(\"~p~n\",[is_integer(\
         erlang:cancel_timer(erlang:send_after(10000, self(), x)))]).",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "true\n");

    let output = beam_rt(&[
        "-noshell",
        "-eval",
        "io:format(\"~p~n\",[erlang:cancel_timer(make_ref())]).",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "false\n");
}

/// `erlang:send_after/3` is a **runtime** capability now, not a Milestone-1 gap: a bogus
/// time or destination is the oracle's `error:badarg` (a genuine runtime error, exit 1),
/// never `unsupported:` (exit 3).
///
/// Oracle: `erlang:send_after(-1, self(), x)` ⇒ `error:badarg` (cause `time`);
/// `erlang:send_after(1, {}, x)` ⇒ `error:badarg`.
#[test]
fn eval_send_after_rejects_a_bad_time_or_destination_as_badarg() {
    for expr in [
        "erlang:send_after(-1, self(), x)",
        "erlang:send_after(1, {}, x)",
    ] {
        let output = beam_rt(&["-noshell", "-eval", expr]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{expr}: stderr {}",
            stderr(&output)
        );
        let message = stderr(&output);
        assert!(message.contains("badarg"), "{expr}: {message}");
        assert!(!message.contains("unsupported"), "{expr}: {message}");
    }
}
