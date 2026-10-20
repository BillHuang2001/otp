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

//! **A closure `erlang:spawn/1` starts from `-eval` is real code** (A.1/A.6/A.7).
//!
//! The `-eval` evaluator is a recursive-descent tree walker, so a closure it builds has the
//! native Rust stack for a continuation and nothing can park it. `src/eval/compile.rs` closes
//! that for the **spawn** route: the closure's own source text is embedded in a synthetic
//! `beam_rt_eval_fun_<index>` module whose parameters are the closure's captured bindings,
//! compiled in process by OTP's own compiler and loaded with `code:load_binary/3`, so the child
//! is an ordinary `ProcessEntry::Export` — a real machine that parks in a bare `receive` and in
//! a nested `timer:sleep/1`, and resumes when a message arrives.
//!
//! Every case below is a `-eval` probe at the **CLI boundary**
//! (`env!("CARGO_BIN_EXE_beam_rt")`), so the whole path runs: the parser's source slicing, the
//! in-process compile, the code server's load, the scheduler, and the `~p` printer. Each probe
//! is compared with the system oracle by **trimmed stdout equality plus the exit code**, and
//! the rendering is also pinned as a golden, so a regression fails even where the toolchain is
//! absent. A missing toolchain is a **loud skip**, never a silent pass.
//!
//! The rendezvous windows are generous (`after 5000`) on purpose: the host is shared and a
//! parked child only needs *one* scheduler pass to be resumed, so no case asserts a latency.
//! Every probe ends with `halt(0)`, so it terminates even when the shape it checks is broken —
//! a broken `receive` prints `timeout` and fails the golden instead of hanging the suite, and
//! `support::oracle_eval` bounds the oracle probe with a kill at [`support::PROBE_TIMEOUT`].
#[allow(dead_code)]
mod support;

use std::path::Path;
use std::process::Command;

/// The suite's name in the loud-skip banner and in every failure message.
const BANNER: &str = "eval_spawn";

/// Runs `beam_rt -noshell -eval <expr>` through the built binary and returns
/// `(trimmed stdout, exit code)`.
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

/// Asserts this runtime prints `golden` for `expr`, that the probe ran to completion, and that
/// the system oracle prints the same bytes.
///
/// `case` names the shape in every failure message. `None` from
/// [`support::oracle_eval`] is reported as a warning — the golden assertion above it has already
/// proved the shape's behaviour on this host, and a probe that cannot answer must not turn into
/// a silent pass *or* a spurious failure.
fn agrees(oracle: &Path, case: &str, expr: &str, golden: &str) {
    let (stdout, exit) = beam_eval(expr);
    assert_eq!(
        exit, 0,
        "{case}: `beam_rt -eval` must exit 0 (a runtime error or a capability gap here means the \
         spawned closure never ran as a real process); stdout {stdout:?}"
    );
    assert_ne!(
        stdout, "timeout",
        "{case}: the rendezvous never completed — the spawned closure was never resumed"
    );
    assert_eq!(stdout, golden, "{case}: this runtime's answer");
    match support::oracle_eval(oracle, None, expr) {
        Some(expected) => assert_eq!(
            stdout, expected,
            "{case}: the spawned closure must behave as the system oracle's does"
        ),
        None => eprintln!("warning: the oracle could not answer `{case}`"),
    }
}

// ---------------------------------------------------------------------------------------
// The gap probe: a spawned blocking closure stays alive
// ---------------------------------------------------------------------------------------

/// The probe the gap was reported with: a spawned closure parked in a bare `receive` is
/// **alive** after its spawner slept, where the tree-walking entry refused the `receive` and
/// killed the child.
///
/// Oracle: `alive=true`.
#[test]
fn a_spawned_blocking_closure_stays_alive() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/alive")) else {
        return;
    };
    let expr = "P = spawn(fun() -> receive M -> M end end), timer:sleep(150), \
                io:format(\"alive=~p~n\", [is_process_alive(P)]), halt(0).";
    agrees(&oracle, "a spawned blocking closure stays alive", expr, "alive=true");
}

/// The core acceptance: the message is delivered by a **later send** and the child's own
/// continuation runs after the park — `{got,hello}`, never `timeout` and never a replay.
///
/// Oracle: `{got,hello}`.
#[test]
fn a_later_message_resumes_the_spawned_closure() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/resume")) else {
        return;
    };
    let expr = "Me = self(), P = spawn(fun() -> receive M -> Me ! {got,M} end end), \
                timer:sleep(50), P ! hello, \
                receive R -> io:format(\"~p~n\", [R]) after 5000 -> io:format(\"timeout~n\") end, \
                halt(0).";
    agrees(&oracle, "a later message resumes the spawned closure", expr, "{got,hello}");
}

/// A nested `timer:sleep/1` — a *call into loaded code* that itself parks on a receive — also
/// survives, because the child is a real engine machine rather than a walker whose nested
/// machine is a stack local.
///
/// Oracle: `done`.
#[test]
fn a_spawned_closure_parks_in_a_nested_timer_sleep() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/nested-park")) else {
        return;
    };
    let expr = "Me = self(), spawn(fun() -> timer:sleep(50), Me ! done end), \
                receive R -> io:format(\"~p~n\", [R]) after 5000 -> io:format(\"timeout~n\") end, \
                halt(0).";
    agrees(&oracle, "a spawned closure parks in a nested timer:sleep", expr, "done");
}

// ---------------------------------------------------------------------------------------
// The capture convention
// ---------------------------------------------------------------------------------------

/// A captured binding is the generated module's **parameter**, so the body reads the value it
/// saw when the closure was created — `5`, the spawner's `N`.
///
/// Oracle: `5`.
#[test]
fn a_captured_binding_reaches_the_spawned_closure() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/capture")) else {
        return;
    };
    let expr = "Me = self(), N = 5, P = spawn(fun() -> receive _ -> Me ! N end end), \
                timer:sleep(50), P ! go, \
                receive R -> io:format(\"~p~n\", [R]) after 5000 -> io:format(\"timeout~n\") end, \
                halt(0).";
    agrees(&oracle, "a captured binding reaches the spawned closure", expr, "5");
}

/// A **match** against the captured value, which is how erl_eval's clause binding behaves: the
/// pattern `X` is already bound, so a message that is not `1` does not match and the child
/// takes its `after` arm (`no2`), while the message `1` does (`ok1`).
///
/// The two answers are sorted so the probe's own message order cannot matter, and the second
/// child's `after 300` window is wide enough that a loaded host cannot turn it into a flake.
///
/// Oracle: `[no2,ok1]`.
#[test]
fn a_message_pattern_matches_against_the_captured_value() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/capture-match")) else {
        return;
    };
    let expr = "Me = self(), X = 1, \
                P1 = spawn(fun() -> receive X -> Me ! ok1 after 5000 -> Me ! no1 end end), \
                P2 = spawn(fun() -> receive X -> Me ! ok2 after 300 -> Me ! no2 end end), \
                timer:sleep(100), P1 ! 1, P2 ! 2, \
                A = receive M1 -> M1 after 5000 -> timeout end, \
                B = receive M2 -> M2 after 5000 -> timeout end, \
                io:format(\"~p~n\", [lists:sort([A,B])]), halt(0).";
    agrees(
        &oracle,
        "a message pattern matches against the captured value",
        expr,
        "[no2,ok1]",
    );
}

/// The closure's own text is embedded **verbatim**, so everything the compiler derives from it
/// comes out right without a hand-written printer: a multi-clause inner fun with a guard, a
/// bound-pattern clause and a fallback, and a **named** fun invoked recursively.
///
/// Oracle: `{zero,3,other,other,0}`.
#[test]
fn a_named_recursive_fun_and_a_guard_clause_work() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/clauses")) else {
        return;
    };
    let expr = "Me = self(), P = spawn(fun() -> \
                  G = fun(0) -> zero; (N) when N > 0, is_integer(N) -> N; (_) -> other end, \
                  FR = fun FR(0) -> 0; FR(N) -> FR(N-1) end, \
                  Me ! {G(0), G(3), G(-1), G(x), FR(4)} end), \
                receive R -> io:format(\"~p~n\", [R]) after 5000 -> io:format(\"timeout~n\") end, \
                halt(0).";
    agrees(
        &oracle,
        "a named recursive fun and a guard clause work",
        expr,
        "{zero,3,other,other,0}",
    );
}

/// Every `end`-terminated construct inside the closure body must survive the source **slice** as
/// well as the compile: a `try`, an `if`, a `begin`, a comprehension and a `case` in one body.
///
/// Oracle: `{caught,b,2,[2,4],1}`.
#[test]
fn a_body_full_of_end_terminated_constructs_compiles() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/slicing")) else {
        return;
    };
    let expr = "Me = self(), P = spawn(fun() -> \
                  R = try hd([]) catch error:_ -> caught end, \
                  I = if 1 > 2 -> a; true -> b end, \
                  B = begin 1 + 1 end, \
                  L = [X*2 || X <- [1,2]], \
                  C = case [1] of [Z] -> Z end, \
                  Me ! {R,I,B,L,C} end), \
                receive R -> io:format(\"~p~n\", [R]) after 5000 -> io:format(\"timeout~n\") end, \
                halt(0).";
    agrees(
        &oracle,
        "a body full of end-terminated constructs compiles",
        expr,
        "{caught,b,2,[2,4],1}",
    );
}

// ---------------------------------------------------------------------------------------
// The anti-replay guarantee
// ---------------------------------------------------------------------------------------

/// **Side effects run exactly once.** The child sends `tick` *before* its park, so a design that
/// re-ran the body when the message arrived would replay that send — and the spawner's second
/// receive would answer `twice` instead of `once`.
///
/// Oracle: `once`.
#[test]
fn a_spawned_closures_side_effects_run_exactly_once() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/exactly-once")) else {
        return;
    };
    let expr = "Me = self(), spawn(fun() -> Me ! tick, receive _ -> ok end end), \
                receive tick -> ok after 5000 -> io:format(\"timeout~n\") end, \
                receive tick -> io:format(\"twice~n\") after 1000 -> io:format(\"once~n\") end, \
                halt(0).";
    agrees(&oracle, "a spawned closure's side effects run exactly once", expr, "once");
}

/// The same function spawned **twice** delivers twice: the compile is cached per closure index,
/// so the second spawn reuses the loaded module — and a cache that answered the wrong entry, or
/// a load that replaced the module under a live child, would show up as a missing second
/// message.
///
/// Oracle: `twice`.
#[test]
fn a_second_spawn_of_the_same_closure_reuses_the_compiled_module() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/cache")) else {
        return;
    };
    let expr = "Me = self(), F = fun() -> Me ! done end, \
                spawn(F), receive done -> ok after 5000 -> io:format(\"timeout~n\") end, \
                spawn(F), receive done -> ok after 5000 -> io:format(\"timeout~n\") end, \
                io:format(\"twice~n\"), halt(0).";
    agrees(
        &oracle,
        "a second spawn of the same closure reuses the compiled module",
        expr,
        "twice",
    );
}

// ---------------------------------------------------------------------------------------
// The spawn family, and the shapes around it that must not regress
// ---------------------------------------------------------------------------------------

/// A **zero-capture** closure compiles to `run/0`; `spawn_monitor/1` of it answers the child's
/// own `normal` exit reason, which also proves the child really ran to completion.
///
/// Oracle: `normal`.
#[test]
fn a_non_blocking_spawned_closure_runs_and_exits_normally() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/no-captures")) else {
        return;
    };
    let expr = "Me = self(), {P, R} = spawn_monitor(fun() -> ok end), \
                receive {'DOWN', R, _, _, W} -> io:format(\"~p~n\", [W]) \
                after 5000 -> io:format(\"timeout~n\") end, halt(0).";
    agrees(
        &oracle,
        "a non-blocking spawned closure runs and exits normally",
        expr,
        "normal",
    );
}

/// `spawn_opt/2`'s `[monitor]` option: the blocking child stays **alive** and the monitor does
/// not fire. The `after 0` arm answers the monitor's own verdict, so `alive true` is the whole
/// claim — no latency is asserted.
///
/// Oracle: `alive true`.
#[test]
fn spawn_opt_monitors_a_blocking_closure_without_killing_it() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/spawn-opt")) else {
        return;
    };
    let expr = "Me = self(), {P, R} = spawn_opt(fun() -> receive M -> Me ! M end end, [monitor]), \
                timer:sleep(100), \
                receive {'DOWN', R, _, _, W} -> io:format(\"down ~p~n\", [W]) \
                after 0 -> io:format(\"alive ~p~n\", [is_process_alive(P)]) end, halt(0).";
    agrees(
        &oracle,
        "spawn_opt monitors a blocking closure without killing it",
        expr,
        "alive true",
    );
}

/// `spawn_link/1` of the same closure: a link must not turn the child's park into an exit.
///
/// Oracle: `linked alive=true`.
#[test]
fn spawn_link_keeps_a_blocking_closure_alive() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/spawn-link")) else {
        return;
    };
    let expr = "P = spawn_link(fun() -> receive M -> M end end), timer:sleep(150), \
                io:format(\"linked alive=~p~n\", [is_process_alive(P)]), halt(0).";
    agrees(
        &oracle,
        "spawn_link keeps a blocking closure alive",
        expr,
        "linked alive=true",
    );
}

/// An uncaught raise inside the spawned closure reaches the monitor with the oracle's reason: the
/// child is a real process, so its death is an ordinary exit.
///
/// Oracle: `boom`.
#[test]
fn an_uncaught_raise_in_a_spawned_closure_reaches_the_monitor() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/raise")) else {
        return;
    };
    let expr = "Me = self(), {P, R} = spawn_monitor(fun() -> exit(boom) end), \
                receive {'DOWN', R, _, _, W} -> io:format(\"~p~n\", [W]) \
                after 5000 -> io:format(\"timeout~n\") end, halt(0).";
    agrees(
        &oracle,
        "an uncaught raise in a spawned closure reaches the monitor",
        expr,
        "boom",
    );
}

/// A **captured fun** applied by loaded code (`lists:map/2`) inside the spawned closure, whose
/// own `self()` is the name a nested `spawn` sends to: the in-slice spawn keeps the tree-walking
/// entry (re-entering the scheduler from inside a running engine is forbidden), that entry is
/// still correct for a body that does not park, and its send resumes the parked outer child.
///
/// Oracle: `{[1,4,9],inner}`.
#[test]
fn a_captured_fun_and_a_nested_spawn_work_inside_a_spawned_closure() {
    let Some(oracle) = support::require_oracle(&format!("{BANNER}/nested")) else {
        return;
    };
    let expr = "Me = self(), F = fun(X) -> X*X end, \
                spawn(fun() -> \
                    Self = self(), \
                    spawn(fun() -> Self ! inner end), \
                    Me ! {lists:map(F, [1,2,3]), receive I -> I after 5000 -> timeout end} end), \
                receive R -> io:format(\"~p~n\", [R]) after 5000 -> io:format(\"timeout~n\") end, \
                halt(0).";
    agrees(
        &oracle,
        "a captured fun and a nested spawn work inside a spawned closure",
        expr,
        "{[1,4,9],inner}",
    );
}

// ---------------------------------------------------------------------------------------
// The fallback: a closure that cannot be compiled
// ---------------------------------------------------------------------------------------

/// A body with an **undefined variable** cannot compile, so the spawn keeps the tree-walking
/// entry and reports that entry's typed error — never a wrong value and never a silently dead
/// child.
///
/// This case is **not** oracle-comparable: `erl -eval` rejects an unbound variable while
/// *parsing* the whole expression, so the oracle exits `1` with no output at all (`erl_parse`'s
/// `variable 'X' is unbound`), while this runtime's `-eval` parser accepts the expression and
/// reports the same class of failure from the evaluator. What is pinned here is this runtime's
/// own honest answer: the child dies (the monitor fires), and the reason names the variable.
#[test]
fn an_uncompilable_closure_still_spawns_and_reports_a_typed_error() {
    let expr = "Me = self(), {P, R} = spawn_monitor(fun() -> Me ! UndefinedVar end), \
                receive {'DOWN', R, _, _, W} -> io:format(\"~p~n\", [W]) \
                after 5000 -> io:format(\"timeout~n\") end, halt(0).";
    let (stdout, exit) = beam_eval(expr);
    assert_eq!(
        exit, 0,
        "the spawn must succeed even though the compile cannot: the child's death is the \
         program's business, not a runtime error; stdout {stdout:?}"
    );
    assert_ne!(
        stdout, "timeout",
        "the child must really die — a declined compile that left it alive would be a silent \
         wrong answer"
    );
    assert!(
        stdout.contains("UndefinedVar") && stdout.contains("is unbound"),
        "the fallback entry must name the unbound variable, got {stdout:?}"
    );
    assert!(
        !stdout.contains("beam_rt_eval_fun"),
        "the compile must have DECLINED (the tree-walking entry answers), not produced a module \
         that fails at run time: {stdout:?}"
    );
}
