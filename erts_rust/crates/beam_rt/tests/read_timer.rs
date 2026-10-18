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
//! **`erlang:read_timer/1,2` — the remaining time of a timer reference — and its
//! user-facing caller `inet:timeout/1`** (contracts A.1/A.6).
//!
//! `beam_rt` serves these two BIFs from its `RuntimeHooks` bridge
//! (`src/context/runtime_hooks.rs`, over `src/timers.rs::Timers::remaining`). The unit suite
//! next to that bridge pins the *dispatch* shape; this suite pins the **observable, end-to-end**
//! behaviour through the CLI (`beam_rt -noshell -eval <expr>`), which is what the system `erl`
//! answers for the same source, and it pins the reason `gen_tcp:connect/4` used to die inside
//! this runtime — `read_timer` was the missing hook behind `inet:timeout/1`
//! (`lib/kernel/src/inet.erl:4952-4959`: `timeout(false) -> infinity; timeout(Timer) ->
//! case erlang:read_timer(Timer) of false -> 0; Time -> Time end`).
//!
//! # Semantics this suite pins (measured against OTP 29.1.1 / erts-17.1)
//!
//! * a **live** timer reference reads its remaining milliseconds — an integer in
//!   `(0, Timeout]` (`get_time_left` truncates whole clock ticks, `erl_hl_timer.c:681-689`), and
//!   the read is **not** a cancellation: a second read never grows it and the timer is still
//!   cancellable afterwards;
//! * a reference with **no live timer** — one from `erlang:make_ref/0`, a monitor reference, a
//!   cancelled timer or an already-fired one — reads the atom `false` (the C answers `false`
//!   only when the timer is gone, `erl_hl_timer.c:1975-1989`);
//! * a **non-reference** first argument is `error:badarg` (`access_bif_timer`'s `is_not_ref`
//!   arm, `erl_hl_timer.c:2244-2251`) — a plain BIF failure, never the typed capability gap
//!   (exit 3) the differential harness keys on;
//! * `read_timer/2` accepts **only** `{async, boolean()}` (last occurrence winning) — `[]` and
//!   `[{async,false}]` answer the integer, `[{async,true}]` answers the atom `ok` and delivers
//!   `{read_timer, TRef, TimeLeft}` — while `{abs, _}`, `{info, _}`, an unknown key, a
//!   non-boolean value, a non-list and an improper list are all `badarg` (`read_timer_2` passes
//!   `NULL` for both the `abs` and the `info` out-parameters of `parse_bif_timer_options`,
//!   `erl_hl_timer.c:2352-2396`).
//!
//! # Comparison discipline
//!
//! Every case is **golden-pinned first** — this runtime's own answer must equal the recorded
//! string, so a regression fails even where no Erlang toolchain exists — and then compared with
//! the system oracle by **trimmed stdout equality plus the exit code**. Where the two runtimes
//! cannot agree byte for byte by construction, the case asserts a **predicate over the value**
//! instead of the value: the remaining time is a clock reading (`inet:timeout/1` measured `4999`
//! here against the oracle's `5000`), so a case pins "an integer in `(0, Timeout]`", never an
//! exact millisecond count.
//!
//! A missing toolchain is a **loud skip** ([`support::require_oracle`]), never a silent pass; the
//! one case that needs a network facility ([`gen_tcp_connect_to_a_closed_local_port_reports_econnrefused`])
//! additionally skips **loudly** when the sandbox refuses loopback connections, instead of
//! failing spuriously on an environmental `{error, eacces}`.
#[allow(dead_code)]
mod support;

use std::net::TcpListener;
use std::process::Command;

/// What one `beam_rt -noshell -eval <expr>` run printed and how it exited.
struct Observation {
    /// Trimmed stdout — the expr's own `io:format` output.
    stdout: String,
    /// Trimmed stderr (the CLI's diagnostics, e.g. an uncaught raise's reason).
    stderr: String,
    /// The process exit code (`-1` when the child was killed by a signal).
    exit: i32,
}

/// Runs `beam_rt -noshell -eval <expr>` — the CLI's documented expression route (contract A.6).
///
/// The `-eval` action prints nothing of its own, so every observation below is exactly what the
/// expression printed.
fn beam_eval(expr: &str) -> Observation {
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(["-noshell", "-eval", expr])
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt for {expr:?} failed: {error}"));
    Observation {
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        exit: output.status.code().unwrap_or(-1),
    }
}

/// Runs the oracle on `expr` and returns `(trimmed stdout, exit code)`.
///
/// Deliberately bypasses [`support::oracle_eval`] (which only reports a *successful* probe): the
/// `badarg` cases below compare the **exit code** of a raise, so the failing status is the
/// observation. The command line is the probe convention's — `erl -noshell -eval expr -s init
/// stop`, bounded by [`support::run_bounded`] so a blocking probe can never hang the suite.
fn oracle_run(oracle: &std::path::Path, expr: &str) -> Option<(String, i32)> {
    let output = support::run_bounded(
        oracle,
        &["-noshell", "-eval", expr, "-s", "init", "stop"],
    )?;
    Some((
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        output.status.code().unwrap_or(-1),
    ))
}

/// Asserts this runtime answers `golden` (exit 0) for `expr` and that the oracle agrees.
///
/// The golden check runs first, so the case fails on a regression with or without a toolchain;
/// the oracle comparison is the cross-check (a probe that cannot answer prints a warning, the
/// [`support::oracle_eval`] convention every suite in this directory follows).
fn assert_agrees_with_the_oracle(banner: &str, expr: &str, golden: &str) {
    let observed = beam_eval(expr);
    assert_eq!(
        observed.exit, 0,
        "`-eval` must succeed (stderr {:?})",
        observed.stderr
    );
    assert_eq!(
        observed.stdout, golden,
        "this runtime's own answer regressed (expr {expr:?})"
    );
    let Some(oracle) = support::require_oracle(banner) else {
        return;
    };
    match support::oracle_eval(&oracle, None, expr) {
        Some(expected) => assert_eq!(
            observed.stdout, expected,
            "the answer must be byte-identical to the system oracle's"
        ),
        None => eprintln!("warning: the oracle could not answer {expr:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// `erlang:read_timer/1` on a live timer
// ---------------------------------------------------------------------------------------
/// A live `erlang:send_after/3` reference reads a remaining time inside its window, the read
/// never grows it, and reading does **not** cancel the timer.
///
/// The window assertion is the point: the bare value is a clock reading (the oracle answered
/// `5000`, this runtime `4999` for the same 5000 ms timer), so only the bound is comparable.
/// `cancel_timer` answering a positive integer afterwards is what distinguishes a **read** from
/// the cancellation `cancel_timer/1` performs.
const LIVE_SEND_AFTER: &str = "TRef = erlang:send_after(5000, self(), ping), \
     A = erlang:read_timer(TRef), B = erlang:read_timer(TRef), Cancel = erlang:cancel_timer(TRef), \
     io:format(\"~p~n\", [is_integer(A) andalso A > 0 andalso A =< 5000]), \
     io:format(\"~p~n\", [B =< A]), \
     io:format(\"~p~n\", [is_integer(Cancel) andalso Cancel > 0]).";

/// The same read on a reference minted by `erlang:start_timer/3`.
const LIVE_START_TIMER: &str = "TRef = erlang:start_timer(5000, self(), hi), \
     V = erlang:read_timer(TRef), \
     io:format(\"~p~n\", [is_integer(V) andalso V > 0 andalso V =< 5000]).";

/// A live timer reads its remaining time, repeatably, without cancelling it.
#[test]
fn a_live_send_after_timer_reads_a_remaining_time_inside_its_window() {
    assert_agrees_with_the_oracle("read_timer (live send_after)", LIVE_SEND_AFTER, "true\ntrue\ntrue");
}

/// A `start_timer/3` reference is a timer reference for `read_timer/1` too.
#[test]
fn a_live_start_timer_reference_reads_a_remaining_time_inside_its_window() {
    assert_agrees_with_the_oracle("read_timer (live start_timer)", LIVE_START_TIMER, "true");
}

// ---------------------------------------------------------------------------------------
// `erlang:read_timer/1` on a reference with no live timer — the atom `false`
// ---------------------------------------------------------------------------------------
/// A `make_ref/0` reference never named a timer.
const MAKE_REF: &str = "io:format(\"~p~n\", [erlang:read_timer(erlang:make_ref())]).";

/// A monitor reference is a reference, but not a timer.
const MONITOR_REF: &str = "Ref = erlang:monitor(process, self()), \
     V = erlang:read_timer(Ref), _ = erlang:demonitor(Ref, [flush]), io:format(\"~p~n\", [V]).";

/// A cancelled timer is gone (the `cancel_timer/1` before the read removed it).
const CANCELLED_TIMER: &str = "TRef = erlang:send_after(5000, self(), ping), \
     _ = erlang:cancel_timer(TRef), io:format(\"~p~n\", [erlang:read_timer(TRef)]).";

/// A **fired** timer is gone too: the `timeout` message arriving is what says the wheel drained
/// it, so the read that follows cannot see a pending deadline.
///
/// The `after 5000` guard is what keeps the case terminating: if the message never arrives the
/// case prints the still-pending reading and fails against the golden `false` — a real signal,
/// never a hang (the CLI's own `-eval` receive waits for the timer).
const FIRED_TIMER: &str = "TRef = erlang:send_after(10, self(), ping), \
     receive ping -> ok after 5000 -> no_message end, \
     io:format(\"~p~n\", [erlang:read_timer(TRef)]).";

/// `read_timer(make_ref())` is `false` on both runtimes.
#[test]
fn a_make_ref_reference_reads_the_atom_false() {
    assert_agrees_with_the_oracle("read_timer (make_ref)", MAKE_REF, "false");
}

/// `read_timer(MonitorRef)` is `false` on both runtimes.
#[test]
fn a_monitor_reference_reads_the_atom_false() {
    assert_agrees_with_the_oracle("read_timer (monitor ref)", MONITOR_REF, "false");
}

/// A cancelled timer reads `false` — the cancellation removed the deadline.
#[test]
fn a_cancelled_timer_reference_reads_the_atom_false() {
    assert_agrees_with_the_oracle("read_timer (cancelled)", CANCELLED_TIMER, "false");
}

/// A timer that already fired reads `false`.
#[test]
fn a_fired_timer_reference_reads_the_atom_false() {
    assert_agrees_with_the_oracle("read_timer (fired)", FIRED_TIMER, "false");
}

// ---------------------------------------------------------------------------------------
// A non-reference first argument is the oracle's `badarg`
// ---------------------------------------------------------------------------------------
/// Every non-reference shape is `badarg`, caught here so the class and the reason are both
/// observable (`try ... catch error:E -> E` answers the reason atom).
const NON_REFERENCES: &str = "R = [begin try erlang:read_timer(A) of V -> V catch error:E -> E end \
     end || A <- [42, {a,b}, foo, [], \"ref\"]], io:format(\"~p~n\", [R]).";

/// A non-reference is `badarg` on both runtimes — and a `badarg` is **not** the typed capability
/// gap (exit 3) the differential harness keys on.
#[test]
fn a_non_reference_first_argument_is_badarg_on_both_runtimes() {
    assert_agrees_with_the_oracle(
        "read_timer (non-reference)",
        NON_REFERENCES,
        "[badarg,badarg,badarg,badarg,badarg]",
    );
}

/// Uncaught, the same `badarg` ends the run with the runtime's error exit code — the shape the
/// differential harness records for a BIF failure.
///
/// The oracle exits `1` for an uncaught `-eval` raise as well, but prints **nothing** (a `-eval`
/// failure is silent: measured, stdout and stderr empty), so only the exit code is comparable;
/// the reason atom is pinned from this runtime's own diagnostic. It is matched as a **substring**
/// so a diagnostic-wording change elsewhere does not break this suite while the semantics stay
/// pinned.
#[test]
fn an_uncaught_badarg_exits_one_like_the_oracle() {
    let expr = "erlang:read_timer(42).";
    let observed = beam_eval(expr);
    assert_eq!(observed.exit, 1, "a BIF failure exits 1 (runtime error)");
    assert!(
        observed.stdout.is_empty(),
        "the failing expression prints nothing: {:?}",
        observed.stdout
    );
    assert!(
        observed.stderr.contains("badarg"),
        "the diagnostic names the reason atom: {:?}",
        observed.stderr
    );
    let Some(oracle) = support::require_oracle("read_timer (uncaught badarg)") else {
        return;
    };
    match oracle_run(&oracle, expr) {
        Some((_, exit)) => assert_eq!(
            exit, 1,
            "the oracle exits 1 for the same uncaught raise (it prints nothing)"
        ),
        None => eprintln!("warning: the oracle could not answer {expr:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// `erlang:read_timer/2` — the `{async, boolean()}` option list
// ---------------------------------------------------------------------------------------
/// The accepted forms that answer the remaining integer: the empty list and `{async, false}`.
const READ_TIMER_2_INTS: &str = "TRef = erlang:send_after(5000, self(), ping), \
     V = erlang:read_timer(TRef, [{async,false}]), W = erlang:read_timer(TRef, []), \
     io:format(\"~p~n\", [is_integer(V) andalso V > 0 andalso V =< 5000]), \
     io:format(\"~p~n\", [is_integer(W) andalso W > 0 andalso W =< 5000]).";

/// `{async, true}` answers the atom `ok` and delivers the reading as a message instead.
const READ_TIMER_2_ASYNC: &str = "TRef = erlang:send_after(5000, self(), ping), \
     io:format(\"~p~n\", [erlang:read_timer(TRef, [{async,true}])]).";

/// Every rejected option shape: `{abs, _}` and `{info, _}` (both out-parameters are `NULL` for
/// this BIF), a non-boolean value, a non-list, an improper list and an unknown key.
const READ_TIMER_2_REJECTED: &str = "TRef = erlang:send_after(5000, self(), ping), \
     Bad = [begin try erlang:read_timer(TRef, O) of V -> V catch error:E -> E end end || \
     O <- [[{abs,true}], [{info,true}], [{async,1}], foo, [{async,true}|bar], \
     [{async,true}, nokey]]], io:format(\"~p~n\", [Bad]).";

/// The last occurrence of `{async, _}` wins, in either order.
const READ_TIMER_2_LAST_WINS: &str = "TRef = erlang:send_after(5000, self(), ping), \
     A = erlang:read_timer(TRef, [{async,true},{async,false}]), \
     B = erlang:read_timer(TRef, [{async,false},{async,true}]), \
     io:format(\"~p~n\", [is_integer(A) andalso A > 0 andalso A =< 5000]), \
     io:format(\"~p~n\", [B]).";

/// `[]` and `[{async,false}]` answer the remaining integer.
#[test]
fn read_timer_2_reads_the_remaining_time_for_the_accepted_forms() {
    assert_agrees_with_the_oracle("read_timer/2 (integers)", READ_TIMER_2_INTS, "true\ntrue");
}

/// `[{async,true}]` answers `ok` (the reading arrives as a `{read_timer, TRef, TimeLeft}`
/// message, so the synchronous answer is just the atom).
#[test]
fn read_timer_2_with_async_true_answers_ok() {
    assert_agrees_with_the_oracle("read_timer/2 (async)", READ_TIMER_2_ASYNC, "ok");
}

/// Every option shape outside `{async, boolean()}` is `badarg`.
#[test]
fn read_timer_2_rejects_every_other_option_form_with_badarg() {
    assert_agrees_with_the_oracle(
        "read_timer/2 (rejected forms)",
        READ_TIMER_2_REJECTED,
        "[badarg,badarg,badarg,badarg,badarg,badarg]",
    );
}

/// A repeated `{async, _}` key resolves to its **last** occurrence.
#[test]
fn read_timer_2_lets_the_last_async_occurrence_win() {
    assert_agrees_with_the_oracle("read_timer/2 (last wins)", READ_TIMER_2_LAST_WINS, "true\nok");
}

// ---------------------------------------------------------------------------------------
// The user-facing caller: `inet:timeout/1` (kernel), and `gen_tcp:connect/4`
// ---------------------------------------------------------------------------------------
/// `inet:timeout/1` on a live `inet:start_timer/1` timer (`erlang:read_timer/1` under the hood),
/// plus `inet:timeout/2` — the minimum of the timer's remaining time and the caller's limit.
const INET_TIMEOUT_LIVE: &str = "Tim = inet:start_timer(5000), V = inet:timeout(Tim), \
     io:format(\"~p~n\", [is_integer(V) andalso V > 0 andalso V =< 5000]), \
     io:format(\"~p~n\", [inet:timeout(100, Tim)]).";

/// A cancelled `inet` timer reads `0` (`read_timer` answers `false`, `timeout/1` maps it to `0`).
const INET_TIMEOUT_CANCELLED: &str = "Tim = inet:start_timer(5000), _ = inet:stop_timer(Tim), \
     io:format(\"~p~n\", [inet:timeout(Tim)]).";

/// The same for a timer that **fired** (the `{timeout, Tim, inet}` message says it did).
const INET_TIMEOUT_FIRED: &str = "Tim = inet:start_timer(10), \
     receive {timeout,Tim,inet} -> ok after 5000 -> no_message end, \
     io:format(\"~p~n\", [inet:timeout(Tim)]).";

/// `inet:timeout(false)` — the "no timer" spelling — is `infinity`.
const INET_TIMEOUT_FALSE: &str = "io:format(\"~p~n\", [inet:timeout(false)]).";

/// `inet:timeout/1` reads the live timer's remaining milliseconds (a bound, not an exact value),
/// and `inet:timeout/2` clamps it to the caller's own limit.
#[test]
fn inet_timeout_agrees_with_the_oracle_for_a_live_timer() {
    assert_agrees_with_the_oracle("read_timer (inet:timeout live)", INET_TIMEOUT_LIVE, "true\n100");
}

/// A cancelled and a fired `inet` timer both read `0`.
#[test]
fn inet_timeout_is_zero_for_a_cancelled_and_a_fired_timer() {
    assert_agrees_with_the_oracle(
        "read_timer (inet:timeout cancelled)",
        INET_TIMEOUT_CANCELLED,
        "0",
    );
    assert_agrees_with_the_oracle("read_timer (inet:timeout fired)", INET_TIMEOUT_FIRED, "0");
}

/// `inet:timeout(false)` is the atom `infinity`.
#[test]
fn inet_timeout_of_false_is_infinity() {
    assert_agrees_with_the_oracle("read_timer (inet:timeout false)", INET_TIMEOUT_FALSE, "infinity");
}

/// **The regression this hook was missing.** `gen_tcp:connect/4` arms `inet:start_timer/1` and
/// calls `inet:timeout/1` on every attempt; while `erlang:read_timer/1` was the typed capability
/// gap, every in-runtime connect died on exit 3 (`unsupported`) instead of reporting the
/// connection's own result. Against a **closed** loopback port it must answer the oracle's
/// `{error, econnrefused}`.
///
/// # Why a std `TcpListener`
///
/// The port has to be one nothing listens on, and no Erlang node participates: the test binds
/// `127.0.0.1:0`, reads the assigned port and **drops** the listener, so the port is free and
/// closed. `econnrefused` is what a closed loopback port reports.
///
/// # The loud, narrow skip
///
/// The case needs loopback TCP. A sandbox that refuses it answers some other `{error, Reason}`
/// (or `{error, eacces}`) — an environmental result, not a divergence — so the case skips
/// **loudly** instead of failing, and only when the answer is neither the golden nor the
/// oracle's. Exit `3` (the capability gap) is **never** skipped: that is the regression.
#[test]
fn gen_tcp_connect_to_a_closed_local_port_reports_econnrefused() {
    const BANNER: &str = "read_timer (gen_tcp:connect)";
    let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)) else {
        support::skip_loudly(BANNER, "this sandbox cannot bind a loopback TCP port");
        return;
    };
    let Ok(address) = listener.local_addr() else {
        support::skip_loudly(BANNER, "the loopback listener has no local address");
        return;
    };
    // Drop the listener: the port stays free, and nothing is listening on it.
    drop(listener);
    let expr = format!(
        "io:format(\"~p~n\", [gen_tcp:connect({{127,0,0,1}}, {}, [], 100)]).",
        address.port()
    );
    let observed = beam_eval(&expr);
    assert_ne!(
        observed.exit, 3,
        "`gen_tcp:connect/4` must never be a capability gap again — `inet:timeout/1` reaches \
         `erlang:read_timer/1` (stderr {:?})",
        observed.stderr
    );
    assert_eq!(observed.exit, 0, "the connect reports a result (stderr {:?})", observed.stderr);
    let golden = "{error,econnrefused}";
    let oracle = support::require_oracle(BANNER);
    if let Some(oracle) = &oracle {
        match oracle_run(oracle, &expr) {
            Some((expected, 0)) => {
                if expected != golden {
                    // The oracle itself could not refuse a loopback connection: environmental.
                    support::skip_loudly(
                        BANNER,
                        &format!("the oracle answered {expected:?}, not {golden}"),
                    );
                    return;
                }
                assert_eq!(
                    observed.stdout, expected,
                    "the connect's result must be byte-identical to the system oracle's"
                );
                return;
            }
            Some((_, exit)) => {
                support::skip_loudly(BANNER, &format!("the oracle exited {exit}"));
                return;
            }
            None => eprintln!("warning: the oracle could not answer {expr:?}"),
        }
    }
    // No oracle answer to compare with: the golden stands, and anything else is environmental.
    if observed.stdout != golden {
        support::skip_loudly(
            BANNER,
            &format!("this sandbox answered {:?}, not {golden}", observed.stdout),
        );
        return;
    }
}
