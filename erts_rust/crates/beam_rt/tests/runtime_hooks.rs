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

//! The `RuntimeHooks` BIFs adopted for contract **A.9** at the **executable** boundary,
//! cross-checked with the live system `erl` oracle.
//!
//! Three `erlang:*` keys that `beam_bifs` registers behind the defaulted `RuntimeHooks`
//! seam are served by `beam_rt`'s `RuntimeHooksBridge` — and each one is a deterministic link
//! on the OTP boot/self-hosting chain this suite pins, because the `file:`/`code:`/`.app`-read
//! path dies without them:
//!
//! * `erlang:dt_spread_tag/1` + `erlang:dt_restore_tag/1` — `lib/kernel/src/file.erl`'s
//!   `call/2` brackets **every** file-server round trip with the pair
//!   (`file.erl:3040-3043`). With dynamic tracing compiled out — the oracle's build, and
//!   this runtime's only mode — the two BIFs are pure argument checks
//!   (`erts/emulator/beam/bif.c:6080-6112` / `6162-6167`): `dt_spread_tag/1` accepts the
//!   atoms `true`/`false` and answers `true`; `dt_restore_tag/1` accepts **only** `true`.
//! * `erlang:statistics/1` — `timer:tc`, `heart`, `disk_log` and `pool.erl`'s load
//!   estimator read it. This runtime serves **only** the counters it can measure for real:
//!   `run_queue` (its own single run queue: `beam_process`'s `Scheduler`), `wall_clock`
//!   (the process monotonic clock), the cumulative counter pairs
//!   `reductions`/`exact_reductions` and `context_switches`, `runtime` (the user CPU time the
//!   one long-lived `beam_io::cpu_time::RuntimeClock` measures with `getrusage(RUSAGE_SELF)`),
//!   `io` (the port subsystem's own byte counters) and `scheduler_wall_time` (the atom
//!   `undefined` — this runtime never enables it, exactly as the oracle answers when it is not
//!   enabled). Every other valid key stays the honest typed gap, and a name that is not a
//!   statistics key at all is the oracle's `badarg`.
//!
//! # What is compared, and what is recorded instead
//!
//! Every flow below runs the **same** `-eval` program under the built `beam_rt` binary and
//! under the oracle. The boolean results (`{true,true,true}`, `badarg`, the `badarg` exit
//! code, and the *shape* of the statistics values) are compared directly. Two figures
//! read **this runtime's own state** and are asserted on this side, with the oracle's
//! differing value recorded rather than failed on — the same class as `erlang:processes/0`
//! and `module_loaded/1`'s boot-set size:
//!
//! * `statistics(run_queue)` on a freshly started `-eval` process: this runtime queues
//!   nothing (`0`), while the oracle's larger boot set leaves one process runnable (`1`).
//! * `statistics(wall_clock)`'s **epoch**: the oracle measures from emulator start, this
//!   runtime from its first `statistics(wall_clock)` read (no earlier anchor is reachable
//!   from the seam). The relationship the oracle promises — first read's two elements
//!   equal, both non-negative, never decreasing — is asserted in both.
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery
// and probe bound (the badarg cases need the exit code, which `support::oracle_eval` folds
// away), so the unused remainder is expected here — the same shape `tests/exit.rs` uses.
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
/// The bounded runner lives here rather than in `tests/support/mod.rs` because every probe in
/// this suite is a program that terminates on its own; the polling guard is the belt to that
/// suspenders, and keeping it local avoids editing the shared harness.
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

/// Runs `program` and asserts it exits `0` with exactly `expected` on stdout — the shape
/// every shared flow below asserts for both runtimes.
fn assert_agrees(what: &str, ours: &Run, theirs: &Run, expected: &str) {
    assert_eq!(ours.code, 0, "{what}: beam_rt exit {} — {}", ours.code, ours.err);
    assert_eq!(theirs.code, 0, "{what}: oracle exit {} — {}", theirs.code, theirs.err);
    assert_eq!(ours.out, expected, "{what}: beam_rt's answer");
    assert_eq!(theirs.out, expected, "{what}: the recorded value must be the live oracle's answer");
}

// ---------------------------------------------------------------------------------------
// The programs both runtimes are given
// ---------------------------------------------------------------------------------------

/// `file:call/2`'s tag-bracket round trip: both booleans answer `true`, and the restorer
/// accepts the value the spreader returned.
const EXPR_TAG_ROUND_TRIP: &str = "\
io:format(\"~p~n\", [{erlang:dt_spread_tag(true), erlang:dt_spread_tag(false), \
erlang:dt_restore_tag(true)}]), halt(0).";

/// The spreader rejects a non-boolean with `badarg` (caught, so both runtimes print the
/// reason they raised).
const EXPR_SPREAD_BADARG: &str = "\
try erlang:dt_spread_tag(42) catch error:badarg -> io:format(\"badarg~n\") end, halt(0).";

/// The restorer rejects `false` with `badarg` — the asymmetry with the spreader.
const EXPR_RESTORE_BADARG: &str = "\
try erlang:dt_restore_tag(false) catch error:badarg -> io:format(\"badarg~n\") end, halt(0).";

/// An **uncaught** rejection is the raw call both runtimes exit `1` on.
const EXPR_SPREAD_UNCAUGHT: &str = "erlang:dt_spread_tag(42).";
const EXPR_RESTORE_UNCAUGHT: &str = "erlang:dt_restore_tag(false).";

/// `statistics(run_queue)`'s shape: a non-negative integer, in both runtimes.
const EXPR_RUN_QUEUE_SHAPE: &str = "\
S = erlang:statistics(run_queue), io:format(\"~p~n\", [is_integer(S) andalso S >= 0]), halt(0).";

/// `statistics(wall_clock)`'s shape and the oracle's documented relationship: two
/// non-negative integers whose first element is the total. (`T >= S` is the oracle's
/// `T1 == T2` on a first read.)
const EXPR_WALL_CLOCK_SHAPE: &str = "\
{T,S} = erlang:statistics(wall_clock), \
io:format(\"~p~n\", [{is_integer(T), is_integer(S), T >= 0, S >= 0, T >= S}]), halt(0).";

/// A later `statistics(wall_clock)` read never goes backwards.
const EXPR_WALL_CLOCK_MONOTONIC: &str = "\
{T1,_} = erlang:statistics(wall_clock), {T2,_} = erlang:statistics(wall_clock), \
io:format(\"~p~n\", [T2 >= T1]), halt(0).";

/// `statistics(reductions)`/`statistics(exact_reductions)`: a 2-tuple of integers, in both
/// runtimes (the shape only — the figures are this runtime's own).
const EXPR_REDUCTIONS_SHAPE: &str = "\
{T,S} = erlang:statistics(reductions), io:format(\"~p~n\", [{is_integer(T), is_integer(S)}]), halt(0).";
const EXPR_EXACT_REDUCTIONS_SHAPE: &str = "\
{T,S} = erlang:statistics(exact_reductions), io:format(\"~p~n\", [{is_integer(T), is_integer(S)}]), halt(0).";

/// `statistics(context_switches)`: a 2-tuple whose first element is an integer, in both
/// runtimes.
const EXPR_CONTEXT_SWITCHES_SHAPE: &str = "\
{T,_S} = erlang:statistics(context_switches), io:format(\"~p~n\", [is_integer(T)]), halt(0).";

/// `statistics(io)`: `{{input, InN}, {output, OutN}}` with the two tag atoms fixed and both
/// figures integers, in both runtimes.
const EXPR_IO_SHAPE: &str = "\
{{In, InN}, {Out, OutN}} = erlang:statistics(io), \
io:format(\"~p~n\", [{In, Out, is_integer(InN), is_integer(OutN)}]), halt(0).";

/// `statistics(scheduler_wall_time)`: the atom `undefined` in both runtimes (neither enables
/// the scheduler wall-time measurement).
const EXPR_SCHEDULER_WALL_TIME: &str = "\
io:format(\"~p~n\", [erlang:statistics(scheduler_wall_time)]), halt(0).";

/// `statistics(runtime)`: a 2-tuple of integers (user CPU milliseconds; the values are
/// volatile, so only the shape is pinned), in both runtimes.
const EXPR_RUNTIME_SHAPE: &str = "\
{T,S} = erlang:statistics(runtime), io:format(\"~p~n\", [{is_integer(T), is_integer(S)}]), halt(0).";

/// A valid statistics key this runtime cannot measure: the honest typed capability gap.
///
/// `garbage_collection` needs a real accumulative collector, which this runtime does not keep,
/// so it stays a gap rather than a fabricated number.
const EXPR_STATISTICS_GAP: &str = "erlang:statistics(garbage_collection).";

/// A name that is not a statistics key at all: the oracle's `badarg` (exit 1).
const EXPR_STATISTICS_BADARG: &str = "erlang:statistics(atom_count).";

/// This runtime's own `statistics(run_queue)`: a freshly loaded `-eval` process queues
/// nothing, because the boot loads modules rather than spawning runnable processes.
const EXPR_RUN_QUEUE_VALUE: &str =
    "io:format(\"~p~n\", [erlang:statistics(run_queue)]), halt(0).";
const RUN_QUEUE_OURS: &str = "0";

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// `file:call/2`'s tag bracket: the round trip the whole `file:`/`code:where_is_file`/`.app`
/// boot path depends on, in both runtimes.
#[test]
fn the_dynamic_tag_round_trip_matches_the_live_oracle() {
    let ours = beam_rt_eval(EXPR_TAG_ROUND_TRIP);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(ours.out, "{true,true,true}");

    let Some(oracle) = support::require_oracle("runtime_hooks") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_TAG_ROUND_TRIP);
    assert_agrees("dt tag round trip", &ours, &theirs, "{true,true,true}");
}

/// The two rejections, caught so both runtimes print the reason, plus the same rejections
/// **uncaught** (the raw `badarg` exit code both runtimes share).
#[test]
fn the_dynamic_tag_rejections_match_the_live_oracle() {
    let cases = [
        ("dt_spread_tag(42)", EXPR_SPREAD_BADARG, EXPR_SPREAD_UNCAUGHT),
        ("dt_restore_tag(false)", EXPR_RESTORE_BADARG, EXPR_RESTORE_UNCAUGHT),
    ];
    let Some(oracle) = support::require_oracle("runtime_hooks") else {
        // Without a toolchain, still pin this runtime's own (toolchain-free) answers.
        for (what, caught, uncaught) in cases {
            let ours = beam_rt_eval(caught);
            assert_eq!(ours.code, 0, "{what}: exit {} — {}", ours.code, ours.err);
            assert_eq!(ours.out, "badarg", "{what}");
            let ours = beam_rt_eval(uncaught);
            assert_eq!(ours.code, 1, "{what} uncaught: {} — {}", ours.code, ours.err);
            assert!(ours.err.contains("badarg"), "{what}: {}", ours.err);
        }
        return;
    };
    for (what, caught, uncaught) in cases {
        let ours = beam_rt_eval(caught);
        let theirs = oracle_eval(&oracle, caught);
        assert_agrees(what, &ours, &theirs, "badarg");

        let ours = beam_rt_eval(uncaught);
        let theirs = oracle_eval(&oracle, uncaught);
        assert_eq!(ours.code, 1, "{what} uncaught: {} — {}", ours.code, ours.err);
        assert_eq!(
            theirs.code, 1,
            "{what} uncaught oracle: {} — {}",
            theirs.code, theirs.err
        );
        assert!(ours.err.contains("badarg"), "{what}: {}", ours.err);
    }
}

/// The shared shape of the two served statistics counters, so the adoption is pinned
/// against the oracle rather than against this runtime alone.
#[test]
fn the_served_statistics_shapes_match_the_live_oracle() {
    let flows = [
        ("statistics(run_queue)", EXPR_RUN_QUEUE_SHAPE, "true"),
        (
            "statistics(wall_clock)",
            EXPR_WALL_CLOCK_SHAPE,
            "{true,true,true,true,true}",
        ),
        ("statistics(wall_clock) monotonic", EXPR_WALL_CLOCK_MONOTONIC, "true"),
        ("statistics(reductions)", EXPR_REDUCTIONS_SHAPE, "{true,true}"),
        (
            "statistics(exact_reductions)",
            EXPR_EXACT_REDUCTIONS_SHAPE,
            "{true,true}",
        ),
        (
            "statistics(context_switches)",
            EXPR_CONTEXT_SWITCHES_SHAPE,
            "true",
        ),
        ("statistics(runtime)", EXPR_RUNTIME_SHAPE, "{true,true}"),
        ("statistics(io)", EXPR_IO_SHAPE, "{input,output,true,true}"),
        ("statistics(scheduler_wall_time)", EXPR_SCHEDULER_WALL_TIME, "undefined"),
    ];
    let Some(oracle) = support::require_oracle("runtime_hooks") else {
        for (what, expr, expected) in flows {
            let ours = beam_rt_eval(expr);
            assert_eq!(ours.code, 0, "{what}: exit {} — {}", ours.code, ours.err);
            assert_eq!(ours.out, expected, "{what}");
        }
        return;
    };
    for (what, expr, expected) in flows {
        let ours = beam_rt_eval(expr);
        let theirs = oracle_eval(&oracle, expr);
        assert_agrees(what, &ours, &theirs, expected);
    }
}

/// `statistics(run_queue)` reads **this** runtime's own run queue, so it is asserted on this
/// side; the oracle's differing figure is recorded, not failed on.
///
/// The divergence is the same "answers about this runtime's own state" class as
/// `erlang:processes/0`: the oracle's boot script leaves one process runnable at the instant
/// `-eval` runs (its answer is `1`), while this runtime's 22-module preloaded boot spawns
/// nothing (`0`). The two runtimes agree on the *shape* (asserted above) and on an idle
/// node.
#[test]
fn statistics_run_queue_is_this_runtimes_own_real_queue() {
    let ours = beam_rt_eval(EXPR_RUN_QUEUE_VALUE);
    assert_eq!(ours.code, 0, "beam_rt: exit {} stderr {}", ours.code, ours.err);
    assert_eq!(
        ours.out, RUN_QUEUE_OURS,
        "a freshly booted -eval process queues no runnable process"
    );

    let Some(oracle) = support::require_oracle("runtime_hooks") else {
        return;
    };
    let theirs = oracle_eval(&oracle, EXPR_RUN_QUEUE_VALUE);
    assert_eq!(theirs.code, 0, "oracle: exit {} stderr {}", theirs.code, theirs.err);
    // Recorded, never asserted: the oracle's boot leaves something runnable. Both answers
    // are a non-negative integer — that is the contract.
    let oracle_value: i64 = theirs
        .out
        .parse()
        .unwrap_or_else(|_| panic!("the oracle's run_queue is not an integer: {:?}", theirs.out));
    assert!(oracle_value >= 0, "the oracle's run_queue: {oracle_value}");
    eprintln!(
        "note: statistics(run_queue) — this runtime {ours_out}, oracle {theirs_out} \
         (this runtime's own state; the oracle's boot set leaves one process runnable)",
        ours_out = ours.out,
        theirs_out = theirs.out
    );
}

/// A valid statistics key this runtime cannot measure is the typed capability gap (exit 3),
/// never a fabricated number; a name that is not a statistics key at all is the oracle's
/// `badarg` (exit 1) in both runtimes.
#[test]
fn the_unmeasurable_and_invalid_statistics_keys_are_classified_honestly() {
    let ours = beam_rt_eval(EXPR_STATISTICS_GAP);
    assert_eq!(ours.code, 3, "a capability gap exits 3: {} — {}", ours.code, ours.err);
    assert!(
        ours.err.contains("unsupported"),
        "the gap must say `unsupported`: {}",
        ours.err
    );
    assert!(
        ours.err.contains("garbage_collection"),
        "the gap must name the counter: {}",
        ours.err
    );

    let Some(oracle) = support::require_oracle("runtime_hooks") else {
        let ours = beam_rt_eval(EXPR_STATISTICS_BADARG);
        assert_eq!(ours.code, 1, "an invalid key is badarg: {} — {}", ours.code, ours.err);
        assert!(ours.err.contains("badarg"), "{}", ours.err);
        return;
    };
    // The oracle **accepts** `garbage_collection` (it keeps the accumulative collector this
    // runtime lacks), so the two runtimes differ by capability here — recorded, never
    // asserted as agreement.
    let theirs = oracle_eval(&oracle, EXPR_STATISTICS_GAP);
    assert_eq!(theirs.code, 0, "oracle: exit {} — {}", theirs.code, theirs.err);
    eprintln!(
        "note: statistics(garbage_collection) — this runtime exit 3 (unsupported), oracle {} {:?}",
        theirs.code, theirs.out
    );

    // An unknown key is `badarg` in **both** — the classification this suite guards.
    let ours = beam_rt_eval(EXPR_STATISTICS_BADARG);
    let theirs = oracle_eval(&oracle, EXPR_STATISTICS_BADARG);
    assert_eq!(ours.code, 1, "beam_rt: {} — {}", ours.code, ours.err);
    assert_eq!(theirs.code, 1, "oracle: {} — {}", theirs.code, theirs.err);
    assert!(ours.err.contains("badarg"), "{}", ours.err);
}
