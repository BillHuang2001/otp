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

//! `erlang:spawn_opt/2,3,4`'s **heap-size options** at the executable boundary.
//!
//! The runtime-native floor (`src/native/process.rs`) parses the option list into a
//! `beam_gc::HeapPolicy`, creates the child with that policy (`ProcessTable::spawn`) and
//! reports it back through `process_info/2`'s heap items; this suite pins the path a user
//! sees: the built `beam_rt` binary against the live system `erl` oracle running the
//! **same** expression.
//!
//! # Why `-eval` and not a compiled fixture
//!
//! These flows read the *spawned child's* `process_info/2` heap items, so the `-eval` process
//! — which owns the process table and spawns into it directly — is the shortest path to them,
//! exactly as `tests/monitor.rs` documents. (`beam_rt run <file>` spawns correctly too; see
//! `tests/spawn_from_slice.rs`.)
//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`) — re-probed, not guessed
//!
//! | option | `process_info(Child, …)` |
//! |---|---|
//! | `[]` / `[link]` / `[{priority, _}]` / `[{max_heap_size, _}]` / `[{message_queue_data, _}]` | `min_heap_size` 233, `min_bin_vheap_size` 46422, `fullsweep_after` 65535 |
//! | `{min_heap_size, 1000}` | 1598 (the growth table rounds it) |
//! | `{min_heap_size, 100}` / `233` | 233 (the `DEFAULT_MIN_HEAP_WORDS` floor) |
//! | `{min_heap_size, 234}` | 376 |
//! | `{min_bin_vheap_size, 500}` / `46000` | 46422 (the default floor wins) |
//! | `{fullsweep_after, 3}` / `0` | the value itself |
//!
//! Every *malformed* option list is the oracle's `badarg` in both runtimes, never the
//! `unsupported` capability signal.
//!
//! # Two known, deliberately unasserted divergences
//!
//! 1. **The `max_heap_size` map's key order.** The oracle renders
//!    `#{error_logger, include_shared_binaries, kill, size}`; this runtime's
//!    `max_heap_size_map` renders `#{kill, error_logger, include_shared_binaries, size}`
//!    (and reports the emulator-global `+hmaxk/-el/-ib` flags rather than the per-map
//!    ones). The `size` entry **is** real, so the suite reads it through
//!    `maps:get(size, element(2, process_info(P, max_heap_size)))` instead of comparing
//!    printed maps. Pre-existing `src/native/memory/mod.rs` behaviour, outside this change.
//! 2. **`process_info(P, message_queue_data)`.** The oracle answers `off_heap` after
//!    `{message_queue_data, off_heap}`; this runtime's queue is always heap-resident and
//!    reports `on_heap`. The **option is accepted** (which is what this change is about),
//!    so the suite asserts acceptance, not the item.
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

// The shared harness is compiled in whole, but this suite uses only its toolchain
// discovery and probe bound (the rejection probes need the exit code, which
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
/// Local (not in `tests/support/mod.rs`) for the reason `tests/monitor.rs` documents: every
/// probe here terminates on its own, so the polling guard is the belt to that suspenders,
/// and keeping it local avoids editing the shared harness.
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

/// A spawn whose child blocks in `receive`, then its three heap items printed as one tuple.
///
/// The child parks immediately (it never terminates), which is what the oracle does too, so
/// the printed items are the ones the child was **created** with.
fn heap_expr(options: &str) -> String {
    format!(
        "P = spawn_opt(fun() -> receive stop -> ok end end, {options}), \
         io:format(\"~p~n\", [{{process_info(P, min_heap_size), \
         process_info(P, min_bin_vheap_size), process_info(P, fullsweep_after)}}]), halt(0)."
    )
}

/// The `(min_heap_size, min_bin_vheap_size, fullsweep_after)` triple for the default
/// process — the base every non-heap option leaves untouched.
const DEFAULTS: &str = "{{min_heap_size,233},{min_bin_vheap_size,46422},{fullsweep_after,65535}}";

/// Every heap-size option form and the exact triple the oracle prints for it.
///
/// The first six rows are the option forms that carry **no** heap value: they must leave
/// the default policy in place (and they prove the newly *accepted* options —
/// `max_heap_size`, `message_queue_data`, `priority` — really are accepted).
const VALUE_FORMS: [(&str, &str, &str); 14] = [
    ("no options at all", "[]", DEFAULTS),
    ("[link]", "[link]", DEFAULTS),
    ("[{priority, high}]", "[{priority, high}]", DEFAULTS),
    (
        "[{max_heap_size, 1000}]",
        "[{max_heap_size, 1000}]",
        DEFAULTS,
    ),
    (
        "[{max_heap_size, #{size => 1000}}]",
        "[{max_heap_size, #{size => 1000}}]",
        DEFAULTS,
    ),
    (
        "[{message_queue_data, off_heap}]",
        "[{message_queue_data, off_heap}]",
        DEFAULTS,
    ),
    (
        "min_heap_size rounds 1000 up through the growth table",
        "[{min_heap_size, 1000}]",
        "{{min_heap_size,1598},{min_bin_vheap_size,46422},{fullsweep_after,65535}}",
    ),
    (
        "min_heap_size below the default floor stays at it",
        "[{min_heap_size, 100}]",
        DEFAULTS,
    ),
    (
        "min_heap_size exactly at the default floor stays at it",
        "[{min_heap_size, 233}]",
        DEFAULTS,
    ),
    (
        "min_heap_size just above the floor is rounded up",
        "[{min_heap_size, 234}]",
        "{{min_heap_size,376},{min_bin_vheap_size,46422},{fullsweep_after,65535}}",
    ),
    (
        "min_bin_vheap_size below the default floor stays at it",
        "[{min_bin_vheap_size, 500}]",
        DEFAULTS,
    ),
    (
        "min_bin_vheap_size just below the floor stays at it",
        "[{min_bin_vheap_size, 46000}]",
        DEFAULTS,
    ),
    (
        "fullsweep_after is taken verbatim",
        "[{fullsweep_after, 3}]",
        "{{min_heap_size,233},{min_bin_vheap_size,46422},{fullsweep_after,3}}",
    ),
    (
        "fullsweep_after 0 disables fullsweeps",
        "[{fullsweep_after, 0}]",
        "{{min_heap_size,233},{min_bin_vheap_size,46422},{fullsweep_after,0}}",
    ),
];

/// A heap option combined with a non-heap one: the child gets **both**.
const EXPR_COMBINED: &str = "\
P = spawn_opt(fun() -> receive stop -> ok end end, [{min_heap_size, 1000}, {priority, high}]), \
io:format(\"~p~n\", [{process_info(P, min_heap_size), process_info(P, priority)}]), \
halt(0).";
const COMBINED_EXPECTED: &str = "{{min_heap_size,1598},{priority,high}}";

/// The `/4` form (`Module:Function(Args)`) through the options as well.
const EXPR_ARITY4: &str = "\
P = spawn_opt(erlang, apply, [fun() -> receive stop -> ok end end, []], \
              [{min_heap_size, 1000}]), \
io:format(\"~p~n\", [{process_info(P, min_heap_size), process_info(P, fullsweep_after)}]), \
halt(0).";
const ARITY4_EXPECTED: &str = "{{min_heap_size,1598},{fullsweep_after,65535}}";

/// `monitor` still answers `{Pid, Ref}` when it rides along with a heap option.
const EXPR_MONITOR: &str = "\
R = spawn_opt(fun() -> receive stop -> ok end end, [monitor, {min_heap_size, 1000}]), \
io:format(\"~p~n\", [{is_tuple(R), tuple_size(R), is_pid(element(1, R)), \
is_reference(element(2, R))}]), halt(0).";
const MONITOR_EXPECTED: &str = "{true,2,true,true}";

/// `max_heap_size` really reaches the child: read the map's `size` entry (the map's *key
/// order* diverges — see the module docs — so the printed map is never compared).
const EXPR_MAX_HEAP_SIZE: &str = "\
P = spawn_opt(fun() -> receive stop -> ok end end, [{max_heap_size, 1000}]), \
io:format(\"~p~n\", [maps:get(size, element(2, process_info(P, max_heap_size)))]), \
halt(0).";

/// The malformed option lists both runtimes reject with `badarg`.
const REJECTIONS: [(&str, &str); 16] = [
    ("a negative heap size", "[{min_heap_size, -1}]"),
    ("a negative fullsweep_after", "[{fullsweep_after, -1}]"),
    ("a negative min_bin_vheap_size", "[{min_bin_vheap_size, -1}]"),
    ("a 3-tuple option", "[{min_heap_size, 1, 2}]"),
    ("an unknown option tuple", "[{bogus, 1}]"),
    ("an unknown bare atom", "[bogus]"),
    ("a non-atom option", "[42]"),
    ("a tuple whose key is not an atom", "[{1, 1}]"),
    ("a non-binary value for min_heap_size", "[{min_heap_size, foo}]"),
    ("an invalid priority", "[{priority, bogus}]"),
    ("an invalid message_queue_data", "[{message_queue_data, bogus}]"),
    (
        "a max_heap_size map with a non-boolean flag",
        "[{max_heap_size, #{size => 1000, kill => bogus}}]",
    ),
    (
        "a max_heap_size map without the mandatory size",
        "[{max_heap_size, #{kill => true}}]",
    ),
    ("a non-list option argument (atom)", "foo"),
    ("a non-list option argument (integer)", "42"),
    ("an improper option list", "[{min_heap_size, 1000}|foo]"),
];

/// The options the oracle accepts and this runtime genuinely cannot honour: the typed
/// capability gap (exit 3), never a fabricated acceptance.
const CAPABILITY_GAPS: [(&str, &str); 2] = [
    ("{scheduler, 1}", "[{scheduler, 1}]"),
    ("{async_dist, true}", "[{async_dist, true}]"),
];

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every heap-size option form prints exactly what the live oracle prints for the same
/// expression — so the child's policy, not the emulator-global default, is what
/// `process_info/2` reports.
#[test]
fn the_heap_size_options_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("spawn_opts") else {
        return;
    };
    for (label, options, expected) in VALUE_FORMS {
        let expr = heap_expr(options);
        let ours = beam_rt_eval(&expr);
        assert_eq!(ours.code, 0, "{label}: exit {} stderr {}", ours.code, ours.err);
        assert!(ours.err.is_empty(), "{label}: stderr {}", ours.err);
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

        let theirs = oracle_eval(&oracle, &expr);
        assert_eq!(
            theirs.code, 0,
            "{label}: the oracle failed: stderr {}",
            theirs.err
        );
        assert_eq!(
            theirs.out, expected,
            "{label}: the recorded triple must be the live oracle's answer"
        );
        assert_eq!(ours.out, theirs.out, "{label}: the two runtimes must agree");
    }
}

/// A heap option combined with `{priority, _}`, the `/4` form and `monitor` all carry the
/// policy and behave identically in both runtimes.
#[test]
fn the_options_compose_with_priority_monitor_and_the_arity_4_form() {
    let Some(oracle) = support::require_oracle("spawn_opts") else {
        return;
    };
    for (label, expr, expected) in [
        ("heap option + priority", EXPR_COMBINED, COMBINED_EXPECTED),
        ("/4 form", EXPR_ARITY4, ARITY4_EXPECTED),
        ("monitor + heap option", EXPR_MONITOR, MONITOR_EXPECTED),
    ] {
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
        assert_eq!(theirs.out, expected, "{label}: the oracle's answer");
    }
}

/// `{max_heap_size, Size}` reaches the child: the map `process_info/2` answers really
/// carries the requested `size` (only that entry is compared — the map's key order is a
/// documented divergence, see the module docs).
#[test]
fn max_heap_size_reaches_the_child() {
    let Some(oracle) = support::require_oracle("spawn_opts") else {
        return;
    };
    let ours = beam_rt_eval(EXPR_MAX_HEAP_SIZE);
    assert_eq!(ours.code, 0, "exit {} stderr {}", ours.code, ours.err);
    assert_eq!(ours.out, "1000", "beam_rt's answer");

    let theirs = oracle_eval(&oracle, EXPR_MAX_HEAP_SIZE);
    assert_eq!(theirs.code, 0, "the oracle failed: stderr {}", theirs.err);
    assert_eq!(theirs.out, "1000", "the oracle's answer");
}

/// Every malformed option list is the oracle's `badarg` here too: exit 1 with
/// `beam_rt: badarg`, never the `unsupported` capability signal — and the live oracle
/// rejects the same expression, so the rejection is the oracle's own.
#[test]
fn malformed_option_lists_are_badarg_in_both_runtimes() {
    let Some(oracle) = support::require_oracle("spawn_opts") else {
        return;
    };
    for (label, options) in REJECTIONS {
        let expr = heap_expr(options);
        let ours = beam_rt_eval(&expr);
        assert_eq!(ours.code, 1, "{label}: expected exit 1, stderr {}", ours.err);
        assert!(ours.err.contains("badarg"), "{label}: stderr {}", ours.err);
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: a badarg is not a capability gap: {}",
            ours.err
        );
        assert!(ours.out.is_empty(), "{label}: nothing may be printed");

        let theirs = oracle_eval(&oracle, &expr);
        assert_ne!(theirs.code, 0, "{label}: the oracle accepted it");
        assert!(
            theirs.err.contains("badarg"),
            "{label}: the oracle's reason should be badarg: {}",
            theirs.err
        );
    }
}

/// The two options the oracle accepts but this runtime cannot honour are the honest typed
/// capability gap: exit 3 with `beam_rt: unsupported: …`, never a fabricated child.
#[test]
fn the_inexpressible_options_are_typed_capability_gaps() {
    for (label, options) in CAPABILITY_GAPS {
        let ours = beam_rt_eval(&heap_expr(options));
        assert_eq!(ours.code, 3, "{label}: expected exit 3, stderr {}", ours.err);
        assert!(
            ours.err.starts_with("beam_rt: unsupported:"),
            "{label}: stderr {}",
            ours.err
        );
    }
}

/// The inverted split: the forms with no `badarg` and no gap succeed **silently** (exit 0,
/// nothing printed) in this runtime, and the oracle accepts the very same expression — so
/// the acceptance is the oracle's, not a local interpretation.
#[test]
fn the_accepted_options_succeed_in_both_runtimes() {
    let Some(oracle) = support::require_oracle("spawn_opts") else {
        return;
    };
    // `is_pid/1` proves a child really exists for each accepted form.
    for options in [
        "[]",
        "[link]",
        "[{priority, max}]",
        "[{priority, low}]",
        "[{max_heap_size, 1000}]",
        "[{max_heap_size, #{size => 1000}}]",
        "[{max_heap_size, #{size => 1000, kill => false}}]",
        "[{message_queue_data, on_heap}]",
        "[{message_queue_data, off_heap}]",
    ] {
        let expr = format!(
            "P = spawn_opt(fun() -> receive stop -> ok end end, {options}), \
             io:format(\"~p~n\", [is_pid(P)]), halt(0)."
        );
        let ours = beam_rt_eval(&expr);
        assert_eq!(ours.code, 0, "{options}: stderr {}", ours.err);
        assert!(ours.err.is_empty(), "{options}: stderr {}", ours.err);
        assert_eq!(ours.out, "true", "{options}: the child must exist");

        let theirs = oracle_eval(&oracle, &expr);
        assert_eq!(
            theirs.code, 0,
            "{options}: the oracle rejected it: stderr {}",
            theirs.err
        );
        assert_eq!(theirs.out, "true", "{options}: the oracle's answer");
    }
}
