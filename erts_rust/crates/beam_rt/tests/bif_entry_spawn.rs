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

//! The **first-entry BIF dispatch** of a spawned process, and the timer wait behind the
//! `-eval` `receive … after T` window.
//!
//! A process entry that names an MFA the **host serves** (`spawn(erlang, send, [Pid, Msg])`)
//! must be dispatched to the BIF, never entered as the module's own code: `erlang.beam`'s
//! `send/2` is the stub `func_info; move undefined -> x0; call_ext_only erlang:nif_error/1`, so
//! entering it raises the bare atom `undefined` and the message is never delivered. That spawn
//! shape is exactly what `timer_server`'s `do_apply/2` runs for a `timer:apply_after/4` MFA, so
//! before the fix a fired timer's message never arrived.
//!
//! The runner now asks the same cross-module-call seam a `call_ext*` uses
//! ([`ExecutionContext::resolve_external_call`]): `Ok(None)` means "the host serves this
//! MFA", and the entry is dispatched through `call_bif`, whose return value becomes the
//! process's `x(0)`. The `-eval` `receive … after T` window additionally waits out its remaining
//! time on the runtime's **timer heap** (`Runtime::wait_for_timer`), so a deadline that falls
//! inside the window fires (and delivers) instead of the `after` body running early.
//!
//! # What each case pins
//!
//! * [`a_spawned_bif_entry_delivers_its_message_on_both_routes`] — `spawn(erlang, send, …)`
//!   delivers, on the `-eval` route **and** the scheduled `run` route (committed fixture
//!   `bifent:fire/0`, `tests/fixtures/bifent.beam`).
//! * [`a_spawned_bif_entrys_return_value_is_the_bifs_result`] — a spawn's return value is not
//!   oracle-observable, so the `Runtime` API is driven directly: `spawn_process("erlang",
//!   "abs", [-3])` must record `3` (`undefined` before the fix).
//! * [`a_fired_timer_delivers_on_both_routes`] — `timer:apply_after(50, erlang, send, …)`
//!   followed by a `receive … after 1500`, on both routes (`tmr:fire/0`).
//! * [`an_already_queued_message_is_still_received_on_both_routes`] — the regression guard: the
//!   timer wait must not delay or drop a message that is already in the mailbox.
//! * [`the_eval_after_window_really_waits`] — a bare `receive _ -> ok after 300 -> ok end` spends
//!   its window instead of returning at once (a **lower bound** only).
//!
//! # Why the `-eval` probes bind the receive's value
//!
//! Erlang's own `-eval` rejects `receive M -> M after 1500 -> timeout end, io:format("~p~n",
//! [M])`: `M` is bound inside a receive whose `after` branch may not bind it, so
//! `erl_eval` terminates with `unsafe_var` (`erl -noshell -eval '<that>' -s init stop` exits
//! 1 with `Failed to eval`). Every probe here therefore binds the value the way the oracle
//! accepts — `M = receive X -> X after 1500 -> timeout end, io:format("~p~n", [M])` — which
//! exercises exactly the same receive/timer path.
//!
//! The oracle is the expected value everywhere it can be observed (a missing `erl` is a
//! **loud** skip, never a silent pass); the two spawn-result/wait cases are asserted from
//! hard-coded expectations because neither is observable in Erlang. No `erlc` is needed: the
//! fixtures are committed `.beam`s, copied into a private scratch directory so the oracle's
//! single `-pa` never points at `tests/fixtures/` (which holds a shadowing `lists.beam`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use beam_rt::Runtime;
use beam_term::{make_small, signed_value};

#[allow(dead_code)]
mod support;

/// The `erl`-facing banner for the loud skip.
const BANNER: &str = "bif-entry spawn";

/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// This crate's fixture directory (the committed `.beam`s live here, never handed to the oracle).
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// `spawn(erlang, send, [self(), done])` then a bounded receive, on the `-eval` route — the
/// `timer_server`'s `do_apply/2` spawn shape, driven directly.
const SPAWN_SEND_EVAL: &str = concat!(
    "spawn(erlang,send,[self(),done]), ",
    "M = receive X -> X after 1500 -> timeout end, io:format(\"~p~n\",[M]), halt(0)."
);

/// The regression guard: a message already queued when the receive starts.
const SELF_SEND_EVAL: &str = concat!(
    "self() ! done, ",
    "M = receive X -> X after 1500 -> timeout end, io:format(\"~p~n\",[M]), halt(0)."
);

/// A fired `timer:apply_after/4` timer (its `timer_server` runs the MFA as a spawned process).
const TIMER_EVAL: &str = concat!(
    "timer:apply_after(50, erlang, send, [self(), done]), ",
    "M = receive X -> X after 1500 -> timeout end, io:format(\"~p~n\",[M]), halt(0)."
);

/// A bare `after 300` window: it must be **waited out**, not skipped.
const AFTER_EVAL: &str = "receive _ -> ok after 300 -> ok end, halt(0).";

/// The `after 300` window the in-process form of [`the_eval_after_window_really_waits`] uses.
const AFTER_EXPR: &str = "receive _ -> ok after 300 -> ok end";

/// How much of the 300 ms window the two timing probes must at least spend.
const AFTER_LOWER_BOUND: Duration = Duration::from_millis(200);

/// The committed `.beam` fixtures copied into the scratch `-pa` (sibling `README.md` holds the
/// `erlc +deterministic` regeneration recipe).
const FIXTURES: [&str; 2] = ["bifent.beam", "tmr.beam"];

// ---------------------------------------------------------------------------------------
// The bounded probes
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` through the shared bounded runner (`/dev/null` stdin, killed at
/// [`support::PROBE_TIMEOUT`]); a probe that cannot run to completion is a **loud failure**,
/// never a silent pass.
fn run(program: &Path, args: &[&str]) -> Run {
    let output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt <args>` through the built binary.
fn beam_rt(args: &[&str]) -> Run {
    run(Path::new(BINARY), args)
}

// ---------------------------------------------------------------------------------------
// The compiled fixtures (copied into a scratch `-pa` that is safe for the oracle)
// ---------------------------------------------------------------------------------------

/// A private scratch directory holding copies of the committed fixtures, removed on drop.
struct Scratch {
    /// The scratch directory: a safe single `-pa` for the oracle (it holds no `lists.beam`).
    dir: PathBuf,
}

impl Scratch {
    /// Copies every committed fixture `.beam` into a fresh private directory.
    ///
    /// A fixture that cannot be copied is a **broken checkout**, not a missing toolchain, so it
    /// panics rather than skipping.
    fn new() -> Scratch {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("beam_rt_bif_entry_{}_{unique}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|error| panic!("cannot prepare {}: {error}", dir.display()));
        for fixture in FIXTURES {
            let from = Path::new(FIXTURE_DIR).join(fixture);
            let to = dir.join(fixture);
            std::fs::copy(&from, &to).unwrap_or_else(|error| {
                panic!(
                    "cannot copy the committed fixture {} to {}: {error}",
                    from.display(),
                    to.display()
                )
            });
        }
        Scratch { dir }
    }

    /// The copied `<module>.beam` inside the scratch directory.
    fn beam(&self, module: &str) -> PathBuf {
        self.dir.join(format!("{module}.beam"))
    }
}

impl Drop for Scratch {
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

/// The live oracle and the scratch fixtures, or `None` after a loud skip naming [`BANNER`].
fn fixtures_or_skip() -> Option<(PathBuf, Scratch)> {
    let oracle = support::require_oracle(BANNER)?;
    Some((oracle, Scratch::new()))
}

// ---------------------------------------------------------------------------------------
// The two routes
// ---------------------------------------------------------------------------------------

/// The **scheduled** invocation `run <scratch>/<module>.beam <module>:<function>/0` through the
/// real executable (the `run` action is banner-free by construction, so no `-noshell` is needed
/// to keep stdout to the one `~w` line).
fn run_scheduled(scratch: &Scratch, module: &str, function: &str) -> Run {
    let beam = scratch.beam(module);
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    let spec = format!("{module}:{function}/0");
    beam_rt(&["run", beam, &spec])
}

/// The **`-eval`** invocation of `expr`, with the booted kernel's banner suppressed (`-noshell`)
/// so stdout is exactly what the expression printed — the same shape the oracle probe uses.
fn run_eval(expr: &str) -> Run {
    beam_rt(&["-noshell", "-eval", expr])
}

/// The live oracle's answer for a committed `module:function/0` fixture — a probe that cannot
/// answer is a **loud failure**, never a silent pass.
fn oracle_fixture_line(oracle: &Path, scratch: &Scratch, module: &str, function: &str) -> String {
    let expr = format!("io:format(\"~w~n\",[{module}:{function}()]), halt(0).");
    support::oracle_eval(oracle, Some(&scratch.dir), &expr)
        .unwrap_or_else(|| panic!("the live oracle could not answer {module}:{function}/0"))
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Asserts a `run`-route probe exits `0`, is silent on stderr and prints the oracle's own line.
fn assert_scheduled_matches(label: &str, ours: &Run, expected: &str) {
    assert_eq!(
        ours.code, 0,
        "{label}: the scheduled run must exit 0, got {}: {}",
        ours.code, ours.err
    );
    assert!(
        !ours.err.contains("unsupported"),
        "{label}: this is not a capability gap: {}",
        ours.err
    );
    assert_eq!(ours.err, "", "{label}: the run action is silent on success");
    assert_eq!(ours.out, expected, "{label}: the scheduled run's answer");
}

/// Asserts an `-eval`-route probe exits `0`, names no capability gap and prints the oracle's own
/// line.
fn assert_eval_matches(label: &str, ours: &Run, expected: &str) {
    assert_eq!(
        ours.code, 0,
        "{label}: the -eval route must exit 0, got {}: {}",
        ours.code, ours.err
    );
    assert!(
        !ours.err.contains("unsupported"),
        "{label}: this is not a capability gap: {}",
        ours.err
    );
    assert_eq!(ours.out, expected, "{label}: the -eval route's answer");
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// `spawn(erlang, send, [self(), done])` delivers `done` on both routes.
///
/// The `-eval` route drives the evaluator's receive/timer wait; the scheduled route runs the
/// committed `bifent:fire/0` through the executable's `run` action, where the *spawned child's*
/// first entry is the BIF-dispatch seam under test. Both must print exactly what the live
/// oracle prints.
#[test]
fn a_spawned_bif_entry_delivers_its_message_on_both_routes() {
    let Some((oracle, scratch)) = fixtures_or_skip() else {
        return;
    };

    // The `-eval` route: the evaluator's own receive must see the message the spawned BIF
    // process delivered.
    let expected = support::oracle_eval(&oracle, None, SPAWN_SEND_EVAL)
        .expect("the live oracle answers the -eval probe");
    assert_eq!(
        expected, "done",
        "the oracle's own answer is the expected value"
    );
    let ours = run_eval(SPAWN_SEND_EVAL);
    assert_eval_matches("spawn(erlang, send, …) on -eval", &ours, &expected);

    // The scheduled route: `bifent:fire/0` is `spawn(erlang, send, [self(), done])` followed by
    // a bounded receive, so its return value is the delivered message.
    let expected = oracle_fixture_line(&oracle, &scratch, "bifent", "fire");
    assert_eq!(
        expected, "done",
        "the oracle's own answer is the expected value"
    );
    let ours = run_scheduled(&scratch, "bifent", "fire");
    assert_scheduled_matches("bifent:fire/0", &ours, &expected);
}

/// A BIF-entry spawn's **return value** is the BIF's result.
///
/// A spawn's result is not observable from Erlang, so the runtime API is driven directly:
/// `spawn_process("erlang", "abs", [-3])` must record `3` in `x(0)`. Before the fix the child
/// was entered at `erlang.beam`'s `abs/1` stub and raised the bare atom `undefined`.
#[test]
fn a_spawned_bif_entrys_return_value_is_the_bifs_result() {
    let mut rt = Runtime::with_defaults().expect("the default runtime");

    let pid = rt
        .spawn_process("erlang", "abs", vec![make_small(-3)])
        .expect("the preloaded `erlang` module exports abs/1");

    // The entry dispatch is synchronous (the host serves the MFA), so one pass runs the child
    // to its `return`.
    rt.run_scheduler()
        .expect("the child's entry runs to its return");

    let result = rt
        .process_result(pid)
        .expect("the BIF-returning process recorded its x(0)");
    assert_eq!(
        signed_value(result),
        3,
        "erlang:abs(-3) is the spawned process's result"
    );
    assert!(
        rt.process_exit(pid).is_none(),
        "a normally returning process exits `normal`, not with a reason"
    );
}

/// A `timer:apply_after/4` timer that fires inside a `receive … after T` window delivers its
/// message on both routes.
///
/// This is the end-to-end shape the fix unblocks: `timer_server`'s `do_apply/2` runs the timer's
/// MFA as `spawn(erlang, send, [Pid, Msg])`, so the message can only arrive once the spawned
/// BIF entry is dispatched as a BIF *and* the runtime actually fires the due deadline.
#[test]
fn a_fired_timer_delivers_on_both_routes() {
    let Some((oracle, scratch)) = fixtures_or_skip() else {
        return;
    };

    let expected = support::oracle_eval(&oracle, None, TIMER_EVAL)
        .expect("the live oracle answers the -eval probe");
    assert_eq!(
        expected, "done",
        "the oracle's own answer is the expected value"
    );
    let ours = run_eval(TIMER_EVAL);
    assert_eval_matches("timer:apply_after/4 on -eval", &ours, &expected);

    let expected = oracle_fixture_line(&oracle, &scratch, "tmr", "fire");
    assert_eq!(
        expected, "done",
        "the oracle's own answer is the expected value"
    );
    let ours = run_scheduled(&scratch, "tmr", "fire");
    assert_scheduled_matches("tmr:fire/0", &ours, &expected);
}

/// The regression guard: a message **already in the mailbox** is still received on both routes.
///
/// The timer wait added to the `-eval` window must not delay, drop or out-prioritise a message
/// that is already queued — the mailbox is scanned before any wait (and the queued message must
/// not arm a deadline that then fires a second time).
#[test]
fn an_already_queued_message_is_still_received_on_both_routes() {
    let Some((oracle, scratch)) = fixtures_or_skip() else {
        return;
    };

    let expected = support::oracle_eval(&oracle, None, SELF_SEND_EVAL)
        .expect("the live oracle answers the -eval probe");
    assert_eq!(
        expected, "done",
        "the oracle's own answer is the expected value"
    );
    let ours = run_eval(SELF_SEND_EVAL);
    assert_eval_matches("self() ! done on -eval", &ours, &expected);

    let expected = oracle_fixture_line(&oracle, &scratch, "bifent", "queued");
    assert_eq!(
        expected, "done",
        "the oracle's own answer is the expected value"
    );
    let ours = run_scheduled(&scratch, "bifent", "queued");
    assert_scheduled_matches("bifent:queued/0", &ours, &expected);
}

/// A bare `receive _ -> ok after 300 -> ok end` really **waits** its window out.
///
/// The in-process form is the discriminating measurement (a CLI probe also pays the ~0.4 s
/// boot, which would satisfy a 200 ms lower bound on its own): `Runtime::eval` runs outside any
/// scheduler slice, so before the fix the window returned at once. Both assertions are
/// **lower bounds** — no exact duration is asserted, and no absolute wall time is compared
/// between runtimes (the host is shared and loaded).
#[test]
fn the_eval_after_window_really_waits() {
    let mut rt = Runtime::with_defaults().expect("the default runtime");

    let started = Instant::now();
    let value = rt
        .eval(AFTER_EXPR)
        .expect("the bare `after 300` window elapses");
    let elapsed = started.elapsed();

    assert_eq!(
        rt.atom_name_of(value),
        Some("ok"),
        "`receive _ -> ok after 300 -> ok end` answers `ok`"
    );
    assert!(
        elapsed >= AFTER_LOWER_BOUND,
        "the in-process `after 300` window must really wait, took {elapsed:?}"
    );

    // The CLI route, as the objective's probe spells it (`-noshell -eval '…'`): a lower bound
    // only, since the child's own boot is part of the measured wall time.
    let started = Instant::now();
    let ours = run_eval(AFTER_EVAL);
    let elapsed = started.elapsed();
    assert_eq!(
        ours.code, 0,
        "the CLI `after 300` probe must exit 0, got {}: {}",
        ours.code, ours.err
    );
    assert_eq!(ours.out, "", "the probe prints nothing");
    assert!(
        elapsed >= AFTER_LOWER_BOUND,
        "the CLI `after 300` probe must really wait, took {elapsed:?}"
    );
}
