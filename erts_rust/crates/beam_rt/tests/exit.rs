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

//! `erlang:exit/2,3` at the **executable** boundary.
//!
//! `src/native/process/exit.rs`'s unit tests pin the BIF layer against the runtime's own
//! process table; this suite pins the path a user sees: the built `beam_rt` binary, the
//! runtime-native floor and the `-eval`/`receive` machinery, compared with the live system
//! `erl` oracle running the **same** program.
//!
//! # Why `-eval` and not a compiled fixture
//!
//! Every flow below is an `-eval` program that spawns a helper, exits it and prints the
//! observed facts — the same program both runtimes are given, with no fixture to compile
//! (the same argument as `tests/monitor.rs`; the scheduler-slice spawn path has its own
//! end-to-end proof in `tests/spawn_from_slice.rs`).
//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`)
//!
//! The four flows print exactly (re-probed — and re-asserted — by
//! [`the_exit_flows_match_the_live_oracle`]):
//!
//! ```text
//! bye            %% exit(Other, bye): the spawnee's DOWN reason is `bye`
//! {true,bye}     %% a trapping spawnee reports {'EXIT', Self, bye}
//! {true,true}    %% exit(Other, normal): true, and the spawnee survives
//! {true,true,true}  %% exit(Port|Ref, _) and exit(Ref, bye): the sentinel targets answer true
//! ```
//!
//! # The forms that are *not* compared against the oracle
//!
//! A **self-directed** exit from the `-eval` process itself is deliberately not a flow: the
//! oracle evaluates `-eval` in the `init` process, which has `trap_exit` set, so
//! `exit(self(), bye)` there delivers a message and answers `true` — the trap flag is the
//! whole difference, and the *trapping* half of it is pinned by the `{true,bye}` flow
//! (a spawnee that sets the flag itself) and by the unit tests. Both runtimes are compared
//! for the form that is flag-independent: `process_flag(trap_exit, true)` first
//! ([`the_adopted_targets_succeed_silently`]).
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

// The shared harness is compiled in whole, but this suite uses only its toolchain
// discovery and probe bound (the error-case probes need the exit code, which
// `support::oracle_eval` folds away), so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed
/// and reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite. `None` when the
/// child could not be spawned, polled or collected.
///
/// The bounded runner lives here rather than in `tests/support/mod.rs` because every probe
/// in this suite is a program that terminates on its own; the polling guard is the belt to
/// that suspenders, and keeping it local avoids editing the shared harness.
fn run_bounded(program: &str, args: &[&str]) -> Option<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    eprintln!("warning: `{program}` outlived the probe bound and was killed");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                eprintln!("warning: cannot poll `{program}`: {error}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args`, failing loudly when the probe could not be run at all.
fn run(program: &str, args: &[&str]) -> Run {
    let output = run_bounded(program, args)
        .unwrap_or_else(|| panic!("`{program}` {args:?} could not be run to completion"));
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(env!("CARGO_BIN_EXE_beam_rt"), &["-noshell", "-eval", expr])
}

/// The same expression under the oracle, terminated by `-s init stop` (an `erl -noshell`
/// never exits on its own).
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    let program = oracle
        .to_str()
        .unwrap_or_else(|| panic!("the oracle path is not UTF-8: {}", oracle.display()));
    run(program, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

// ---------------------------------------------------------------------------------------
// The programs both runtimes are given
// ---------------------------------------------------------------------------------------

/// `exit(Other, bye)`: the spawnee is monitored, so its `{'DOWN', …}` carries the reason the
/// signal terminated it with.
const EXPR_OTHER_BYE: &str = r#"
{Pid, Ref} = spawn_opt(fun() -> exit(self(), bye) end, [monitor]),
receive {'DOWN', Ref, process, _, Reason} -> io:format("~p~n", [Reason])
after 2000 -> io:format("timeout~n") end,
halt(0).
"#;

/// A **trapping** spawnee: `exit(self(), bye)` delivers `{'EXIT', Self, bye}` into its own
/// mailbox, so the process survives and reports what it trapped (the parent is reached by
/// its registered name, because a closure cannot capture it here).
const EXPR_TRAPPING_SELF: &str = r#"
register(evalparent, self()),
spawn(fun() ->
    process_flag(trap_exit, true),
    exit(self(), bye),
    receive {'EXIT', From, Reason} -> whereis(evalparent) ! {From =:= self(), Reason}
    after 500 -> whereis(evalparent) ! no_exit_message end
end),
Obs = receive X -> X after 2000 -> no_message end,
io:format("~p~n", [Obs]),
halt(0).
"#;

/// `exit(Other, normal)`: answers `true` and leaves the spawnee alive.
const EXPR_OTHER_NORMAL: &str = r#"
P = spawn(fun() -> receive stop -> ok end end),
io:format("~p~n", [{exit(P, normal), is_process_alive(P)}]),
halt(0).
"#;

/// The targets that hold no signal — an internal port and a reference — answer `true` for
/// **any** reason (the oracle's own taxonomy, `bif.c:1750-1863`).
const EXPR_SENTINELS: &str = r#"
Port = open_port({spawn, "true"}, []),
Ref = make_ref(),
io:format("~p~n", [{exit(Port, normal), exit(Ref, normal), exit(Ref, bye)}]),
halt(0).
"#;

/// `exit/3`'s legal option lists behave exactly like `exit/2`.
const EXPR_EXIT_THREE: &str = r#"
P = spawn(fun() -> receive stop -> ok end end),
io:format("~p~n", [{exit(P, bye, []), exit(P, bye, [priority])}]),
halt(0).
"#;

/// The flows and the value both runtimes must print for them.
const FLOWS: [(&str, &str, &str); 5] = [
    ("exit(Other, bye) -> the DOWN reason", EXPR_OTHER_BYE, "bye"),
    (
        "a trapping spawnee traps its own exit",
        EXPR_TRAPPING_SELF,
        "{true,bye}",
    ),
    (
        "exit(Other, normal) leaves the spawnee alive",
        EXPR_OTHER_NORMAL,
        "{true,true}",
    ),
    ("exit(Port|Ref, _) answers true", EXPR_SENTINELS, "{true,true,true}"),
    ("exit/3 [] and [priority]", EXPR_EXIT_THREE, "{true,true}"),
];

/// The malformed calls both runtimes reject with `badarg`.
const REJECTIONS: [(&str, &str); 8] = [
    (
        "exit(RegisteredName, normal)",
        "register(exit_probe_name, self()), erlang:exit(exit_probe_name, normal)",
    ),
    ("exit(unregistered_atom, normal)", "erlang:exit(exit_probe_unregistered, normal)"),
    ("exit(42, normal)", "erlang:exit(42, normal)"),
    ("exit({a, b}, normal)", "erlang:exit({a, b}, normal)"),
    ("exit/3 option [foo]", "erlang:exit(self(), bye, [foo])"),
    (
        "exit/3 option [{priority, high}]",
        "erlang:exit(self(), bye, [{priority, high}])",
    ),
    (
        "exit/3 option [priority, foo]",
        "erlang:exit(self(), bye, [priority, foo])",
    ),
    ("exit/3 non-list options", "erlang:exit(self(), bye, foo)"),
];

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every flow prints exactly what the live oracle prints for the same program: the DOWN
/// reason of a signalled spawnee, a trapping spawnee's `{'EXIT', …}`, the survival of a
/// `normal` target, the sentinel targets and the `exit/3` option lists.
#[test]
fn the_exit_flows_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("exit") else {
        return;
    };
    for (label, expr, expected) in FLOWS {
        let ours = beam_rt_eval(expr);
        assert_eq!(ours.code, 0, "{label}: exit {} stderr {}", ours.code, ours.err);
        assert!(ours.err.is_empty(), "{label}: stderr {}", ours.err);
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

        let theirs = oracle_eval(&oracle, expr);
        assert_eq!(
            theirs.code, 0,
            "{label}: the oracle failed: stderr {}",
            theirs.err
        );
        assert_eq!(
            theirs.out, expected,
            "{label}: the recorded value must be the live oracle's answer"
        );
    }
}

/// Every malformed call is the oracle's `badarg` here too: exit 1 with `beam_rt: badarg`,
/// never the `unsupported` capability signal — and the live oracle rejects the same program,
/// so the rejection is the oracle's own, not a local interpretation.
#[test]
fn malformed_exit_calls_are_badarg_in_both_runtimes() {
    let Some(oracle) = support::require_oracle("exit") else {
        return;
    };
    for (label, expr) in REJECTIONS {
        let ours = beam_rt_eval(expr);
        assert_eq!(ours.code, 1, "{label}: expected exit 1, stderr {}", ours.err);
        assert!(ours.err.contains("badarg"), "{label}: stderr {}", ours.err);
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: a badarg is not a capability gap: {}",
            ours.err
        );

        let theirs = oracle_eval(&oracle, expr);
        assert_ne!(theirs.code, 0, "{label}: the oracle accepted it");
        assert!(
            theirs.err.contains("badarg"),
            "{label}: the oracle's reason should be badarg: {}",
            theirs.err
        );
    }
}

/// The forms that *are* adopted succeed silently (exit 0, nothing printed): a reference and a
/// port target, a pid that is only signalled with `normal`, and a **trapping** caller's
/// self-directed exit (the one self-exit both runtimes answer `true` for, because the trap
/// flag — not the implementation — decides it).
#[test]
fn the_adopted_targets_succeed_silently() {
    for expr in [
        "erlang:exit(make_ref(), normal).",
        "erlang:exit(open_port({spawn, \"true\"}, []), normal).",
        "P = spawn(fun() -> receive stop -> ok end end), erlang:exit(P, normal).",
        "process_flag(trap_exit, true), erlang:exit(self(), bye).",
        "process_flag(trap_exit, true), erlang:exit(self(), bye, [priority]).",
    ] {
        let run = beam_rt_eval(expr);
        assert_eq!(run.code, 0, "{expr}: stderr {}", run.err);
        assert!(run.out.is_empty(), "{expr}: stdout {}", run.out);
        assert!(run.err.is_empty(), "{expr}: stderr {}", run.err);
    }
}

/// A non-trapping caller's `exit(self(), Reason)` terminates it with that reason: the CLI
/// reports the exit-class reason (exit 1), and the runtime never fabricates a `true` for a
/// caller that is about to die. `kill` is untrappable and always reports `killed`.
#[test]
fn a_non_trapping_self_exit_terminates_with_its_reason() {
    let terminated = beam_rt_eval("erlang:exit(self(), goodbye).");
    assert_eq!(terminated.code, 1, "stderr {}", terminated.err);
    assert_eq!(terminated.out, "", "stdout {}", terminated.out);
    assert!(
        terminated.err.contains("goodbye"),
        "the reason must name itself: {}",
        terminated.err
    );

    let killed = beam_rt_eval("erlang:exit(self(), kill).");
    assert_eq!(killed.code, 1, "stderr {}", killed.err);
    assert!(
        killed.err.contains("killed"),
        "kill is reported as `killed`: {}",
        killed.err
    );

    // `kill` is untrappable: the trap flag does not save the caller.
    let trapping = beam_rt_eval("process_flag(trap_exit, true), erlang:exit(self(), kill).");
    assert_eq!(trapping.code, 1, "stderr {}", trapping.err);
    assert!(
        trapping.err.contains("killed"),
        "a trapping caller is still killed: {}",
        trapping.err
    );
}
