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

//! `erlang:monitor/2,3` and `erlang:demonitor/1,2` at the **executable** boundary.
//!
//! `src/native/process/monitor.rs`'s unit tests pin the BIF layer against the runtime's own
//! process table; this suite pins the path a user sees: the built `beam_rt` binary, the
//! runtime-native floor and the `-eval`/`receive` machinery, compared with the live system
//! `erl` oracle running the **same** expression.
//!
//! # Why `-eval` and not a compiled fixture
//!
//! A monitor only makes sense for a process that can receive its `{'DOWN', …}`, and the two
//! ways to get such a process are a `spawn` or a second scheduler process. Every flow below
//! is therefore an `-eval` program that spawns a helper, monitors it and prints the observed
//! facts — the same program both runtimes are given, with no fixture to compile. (`beam_rt
//! run <file>` spawns correctly too; see `tests/spawn_from_slice.rs`.)
//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`)
//!
//! The three flows print exactly (re-probed and re-asserted by
//! [`the_monitor_flows_match_the_live_oracle`]):
//!
//! ```text
//! {true,true,true}    %% monitor/2 on a pid and on a registered name both answer a ref
//! {true,false,true}   %% demonitor/2 [info]: true then false; demonitor/1: true
//! {true,none}         %% demonitor/2 [flush]: true, and the queued DOWN is gone
//! ```
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

/// `monitor/2` on a live pid and on a registered name: both answer a fresh reference.
const EXPR_REFS: &str = "\
P = spawn(fun() -> receive stop -> ok end end), \
true = register(monslave, P), \
RP = erlang:monitor(process, P), \
RN = erlang:monitor(process, monslave), \
io:format(\"~p~n\", [{is_reference(RP), is_reference(RN), RP =/= RN}]), \
halt(0).";

/// `demonitor/2`'s `[info]` answers the **real** boolean: `true` while the monitor was on
/// record, `false` once it is gone; `demonitor/1` always answers `true`.
const EXPR_LIFECYCLE: &str = "\
P = spawn(fun() -> receive stop -> ok end end), \
R = erlang:monitor(process, P), \
First = erlang:demonitor(R, [info]), \
Second = erlang:demonitor(R, [info]), \
Plain = erlang:demonitor(R), \
io:format(\"~p~n\", [{First, Second, Plain}]), \
halt(0).";

/// `demonitor/2`'s `[flush]` removes a queued `{'DOWN', …}`: the helper is stopped, the
/// probing process blocks on an unrelated message (which lets the helper run to completion
/// and the DOWN land in the mailbox), then the flush removes it and the follow-up receive
/// of the DOWN finds nothing.
const EXPR_FLUSH: &str = "\
P = spawn(fun() -> receive stop -> ok end end), \
R = erlang:monitor(process, P), \
P ! stop, \
receive never -> ok after 50 -> ok end, \
Flushed = erlang:demonitor(R, [flush]), \
Leftover = receive {'DOWN', R, _, _, _} -> down after 0 -> none end, \
io:format(\"~p~n\", [{Flushed, Leftover}]), \
halt(0).";

/// The three flows and the value both runtimes must print for them.
const FLOWS: [(&str, &str, &str); 3] = [
    ("monitor/2 on a pid and on a registered name", EXPR_REFS, "{true,true,true}"),
    (
        "demonitor/1 and demonitor/2 [info]",
        EXPR_LIFECYCLE,
        "{true,false,true}",
    ),
    (
        "demonitor/2 [flush] drops a queued DOWN",
        EXPR_FLUSH,
        "{true,none}",
    ),
];

/// The malformed calls both runtimes reject with `badarg`.
const REJECTIONS: [(&str, &str); 11] = [
    ("monitor(process, 42)", "erlang:monitor(process, 42)"),
    ("monitor(bogus, self())", "erlang:monitor(bogus, self())"),
    ("monitor(time_offset, self())", "erlang:monitor(time_offset, self())"),
    (
        "monitor(process, Port)",
        "erlang:monitor(process, open_port({spawn,\"true\"}, []))",
    ),
    ("monitor/3 option [foo]", "erlang:monitor(process, self(), [foo])"),
    ("monitor/3 non-list options", "erlang:monitor(process, self(), foo)"),
    (
        "monitor/3 option {alias, bogus}",
        "erlang:monitor(process, self(), [{alias, bogus}])",
    ),
    (
        "monitor/3 option 3-tuple {tag, x, extra}",
        "erlang:monitor(process, self(), [{tag, x, extra}])",
    ),
    ("demonitor(42)", "erlang:demonitor(42)"),
    (
        "demonitor/2 option [bogus]",
        "erlang:demonitor(make_ref(), [bogus])",
    ),
    (
        "demonitor/2 non-list options",
        "erlang:demonitor(make_ref(), foo)",
    ),
];

/// The target/option forms the oracle accepts and this runtime **adopts**: a port
/// monitor (recorded on the runtime's authoritative port table — `beam_io`'s
/// `PortRegistry`, the one `open_port/2` allocates from), a per-monitor `{tag, _}`, and
/// the three `{alias, _}` modes whose monitor reference doubles as a **reference alias**
/// (a valid `send` destination — pinned end to end by `tests/alias.rs`).
const ADOPTED_TARGETS: [(&str, &str); 5] = [
    (
        "monitor(port, Port)",
        "erlang:monitor(port, open_port({spawn,\"true\"}, []))",
    ),
    (
        "monitor/3 option {tag, x}",
        "erlang:monitor(process, self(), [{tag, x}])",
    ),
    (
        "monitor/3 option {alias, explicit_unalias}",
        "erlang:monitor(process, self(), [{alias, explicit_unalias}])",
    ),
    (
        "monitor/3 option {alias, demonitor}",
        "erlang:monitor(process, self(), [{alias, demonitor}])",
    ),
    (
        "monitor/3 option {alias, reply_demonitor}",
        "erlang:monitor(process, self(), [{alias, reply_demonitor}])",
    ),
];

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every flow prints exactly what the live oracle prints for the same expression: the
/// reference-answering forms, `demonitor/2`'s real `[info]` boolean and `[flush]`'s removal
/// of a queued `{'DOWN', …}`.
#[test]
fn the_monitor_flows_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("monitor") else {
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
/// never the `unsupported` capability signal — and the live oracle rejects the same
/// expression, so the rejection is the oracle's own, not a local interpretation.
#[test]
fn malformed_monitor_and_demonitor_calls_are_badarg_in_both_runtimes() {
    let Some(oracle) = support::require_oracle("monitor") else {
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

/// The forms the oracle accepts and this runtime now **adopts** — a port monitor, a per-monitor
/// `{tag, _}` and the three `{alias, _}` modes — succeed silently (exit 0, nothing printed), and
/// the live oracle answers a reference for each, so the two runtimes agree rather than both
/// rejecting. (The alias modes are exercised as *send* destinations end to end by
/// `tests/alias.rs`; here only their acceptance is pinned.)
#[test]
fn the_formerly_inexpressible_targets_are_adopted_like_the_oracle() {
    let Some(oracle) = support::require_oracle("monitor") else {
        return;
    };
    for (label, expr) in ADOPTED_TARGETS {
        let ours = beam_rt_eval(expr);
        assert_eq!(ours.code, 0, "{label}: expected exit 0, stderr {}", ours.err);
        assert_eq!(ours.out, "", "{label}: a monitor prints nothing");
    }
    // The oracle answers a reference for both, which is what these cases compare against.
    for (label, expr) in ADOPTED_TARGETS {
        let accepted = format!("io:format(\"~p~n\", [is_reference({expr})]), halt(0).");
        let theirs = oracle_eval(&oracle, &accepted);
        assert_eq!(
            theirs.code, 0,
            "{label}: the oracle should accept it: stderr {}",
            theirs.err
        );
        assert_eq!(theirs.out, "true", "{label}: the oracle answers a reference");
    }

    // A `{Name, Node}` pair naming **another** node needs distribution (contract A.5),
    // which this runtime does not implement: the same typed gap, never a local monitor.
    let remote = beam_rt_eval("erlang:monitor(process, {some_name, 'other@host'})");
    assert_eq!(remote.code, 3, "a remote monitor: stderr {}", remote.err);
    assert!(
        remote.err
            .contains("unsupported: erlang:monitor/2 on a {Name, Node} of another node"),
        "a remote monitor is a capability gap: {}",
        remote.err
    );
}

/// The forms that *are* adopted succeed silently (exit 0, nothing printed) — a monitor on
/// the caller itself and on an unregistered name (the oracle still answers a reference,
/// which then fires `noproc`).
#[test]
fn the_adopted_targets_succeed_silently() {
    for expr in [
        "erlang:monitor(process, self())",
        "erlang:monitor(process, some_unregistered_name)",
        "erlang:monitor(process, self(), [])",
        "erlang:demonitor(make_ref())",
        "erlang:demonitor(make_ref(), [])",
    ] {
        let run = beam_rt_eval(expr);
        assert_eq!(run.code, 0, "{expr}: stderr {}", run.err);
        assert!(run.out.is_empty(), "{expr}: stdout {}", run.out);
        assert!(run.err.is_empty(), "{expr}: stderr {}", run.err);
    }
}
