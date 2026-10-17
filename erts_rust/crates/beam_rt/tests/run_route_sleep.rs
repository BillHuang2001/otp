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

//! The `run` route's **sleep accounting**, end to end (the ping-pong shape).
//!
//! A two-process ping-pong — `Worker ! {self(), N}` then a blocking
//! `receive {_, N}` — empties the run queue between every iteration, so the target
//! process is parked and the scheduler reaches its pass boundary over and over. The two
//! properties that make that shape fast are opposite in direction, and each is pinned here
//! by the **syscall** the runtime issues rather than by a wall time (a shared host cannot
//! be trusted with a timing):
//!
//! * **no poll sleep while the run route has work** — a pass that slept before looking for
//!   a woken receiver would pay that sleep on every iteration, so `clock_nanosleep` must be
//!   **absent** across the whole run (the regression this pins cost ≈25 ms per pass);
//! * **the anti-spin half is intact** — a process parked on a finite `receive … after T`
//!   with nothing else to run *must* be slept to, so the same syscall must appear for
//!   `recv_after:after20/0`.
//!
//! The ping-pong fixture (`tests/fixtures/pingpong.beam`, source + regeneration recipe in
//! `tests/fixtures/README.md`) is a committed oracle fixture: the value it prints is the
//! same under the system `erl`, compared below whenever a toolchain is present.
//!
//! The syscall half needs `strace`; it is skipped **loudly** (never silently passed) when
//! the tool is missing, like the other tool-driven suites in this crate.
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod support;

use support::{find_on_path, is_executable, oracle_eval, require_oracle, run_bounded, skip_loudly};

/// The built runtime — the `run` action is what this suite drives.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The committed ping-pong fixture (`erlc +deterministic pingpong.erl`).
const PINGPONG_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/pingpong.beam"
);

/// The committed finite-`receive` fixture (`erlc +deterministic recv_after.erl`).
const RECV_AFTER_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/recv_after.beam"
);

/// Round trips for the syscall sweep: enough to cross many pass boundaries (a pass covers
/// `RUN_STEP_SLICES = 1000` slices, i.e. ≈500 round trips, so this is ≈40 passes) while
/// staying inside the probe bound on a debug build.
const PINGPONG_ROUNDS: usize = 20_000;

fn binary() -> &'static Path {
    Path::new(BINARY)
}

/// A per-process, per-call scratch path for `strace -o` (never inside the checkout).
fn scratch_trace_path(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "beam_rt_run_sleep_{tag}_{}_{nonce}",
        std::process::id()
    ))
}

/// The runtime's own answer for the ping-pong, and the oracle's — the fixture is a real
/// program, so a divergence here would make every syscall result below meaningless.
#[test]
fn the_run_route_completes_a_two_process_ping_pong_like_the_oracle() {
    let rounds = "1000";
    let output = run_bounded(
        binary(),
        &[
            "run",
            PINGPONG_BEAM,
            "pingpong:go/1",
            rounds,
        ],
    )
    .expect("the built beam_rt runs the ping-pong within the probe bound");
    assert!(
        output.status.success(),
        "run route failed: {output:?}"
    );
    let ours = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(ours, rounds, "the ping-pong's own count comes back");

    let Some(oracle) = require_oracle("run_route_sleep") else {
        return;
    };
    // The oracle needs the module on its code path, and `tests/fixtures/` must never be
    // handed to it as a whole `-pa` (a fixture directory is a grab bag of unrelated
    // modules, and a shadowing `lists.beam` there would take stdlib's place), so the one
    // `.beam` under test is staged into a directory of its own.
    let stage = scratch_trace_path("oracle_dir");
    std::fs::create_dir_all(&stage).expect("a scratch code-path directory");
    let staged = stage.join("pingpong.beam");
    std::fs::copy(PINGPONG_BEAM, &staged).expect("the ping-pong fixture is staged");
    let expr = format!("io:format(\"~w~n\", [pingpong:go({rounds})]), halt(0).");
    let theirs = oracle_eval(&oracle, Some(&stage), &expr);
    let _ = std::fs::remove_dir_all(&stage);
    let Some(theirs) = theirs else {
        skip_loudly("run_route_sleep", "the oracle did not answer the probe");
        return;
    };
    assert_eq!(
        theirs, ours,
        "the runtime and the oracle agree on the ping-pong's value"
    );
}

/// **The sleep accounting**: zero `clock_nanosleep` across a 20 000-round-trip ping-pong,
/// and at least one for a process parked on `receive … after 20`.
#[test]
fn the_run_route_never_sleeps_while_it_has_work_and_still_sleeps_to_a_receive_timeout() {
    let Some(strace) = find_on_path("strace") else {
        skip_loudly(
            "run_route_sleep",
            "no `strace` on $PATH (the syscall half needs it)",
        );
        return;
    };
    assert!(is_executable(&strace));

    // 1. The ping-pong: the run queue never empties, so the pass must not sleep.
    let trace = scratch_trace_path("pingpong");
    let rounds = PINGPONG_ROUNDS.to_string();
    let output = run_bounded(
        &strace,
        &[
            "-f",
            "-e",
            "trace=clock_nanosleep",
            "-o",
            &trace.to_string_lossy(),
            BINARY,
            "run",
            PINGPONG_BEAM,
            "pingpong:go/1",
            &rounds,
        ],
    )
    .expect("the straced ping-pong finishes within the probe bound");
    assert!(
        output.status.success(),
        "the straced run route failed: {output:?}"
    );
    let sleeps = std::fs::read_to_string(&trace).expect("strace wrote its log");
    let _ = std::fs::remove_file(&trace);
    let count = sleeps.matches("clock_nanosleep").count();
    assert_eq!(
        count, 0,
        "the run route has work to do and must not sleep; {count} clock_nanosleep call(s):\n{sleeps}"
    );

    // 2. The anti-spin half: a parked finite `receive … after T` with nothing to run is
    //    slept to (after20 parks for 20 ms), so the same syscall must appear.
    let trace = scratch_trace_path("recv_after");
    let output = run_bounded(
        &strace,
        &[
            "-f",
            "-e",
            "trace=clock_nanosleep",
            "-o",
            &trace.to_string_lossy(),
            BINARY,
            "run",
            RECV_AFTER_BEAM,
            "recv_after:after20/0",
        ],
    )
    .expect("the straced receive-after finishes within the probe bound");
    assert!(
        output.status.success(),
        "the straced receive-after run failed: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "timeout",
        "`after 20` on an empty mailbox times out"
    );
    let sleeps = std::fs::read_to_string(&trace).expect("strace wrote its log");
    let _ = std::fs::remove_file(&trace);
    let count = sleeps.matches("clock_nanosleep").count();
    assert!(
        count >= 1,
        "a process parked on a finite receive must be slept to, not spun on; no clock_nanosleep:\n{sleeps}"
    );
}
