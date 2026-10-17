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

//! The `run` route's **wait accounting**, end to end.
//!
//! A two-process ping-pong — `Worker ! {self(), N}` then a blocking
//! `receive {_, N}` — empties the run queue between every iteration, so the target
//! process is parked and the scheduler reaches its pass boundary over and over. The two
//! properties that make that shape fast are opposite in direction, and each is pinned here
//! by what the pass **does with its time** rather than by a wall time (a shared host cannot be
//! trusted with a timing):
//!
//! * **no blocking wait while the run route has work** — a pass that waited before looking
//!   for a woken receiver would pay that wait on every iteration, so the number of blocking
//!   waits must **not grow with the work done** (the regression this pins cost one 25 ms
//!   poll slice per pass: 116 waits for 2 000 round trips, 1 063 for 20 000). The count is
//!   measured at two sizes, so a per-pass *or* per-round wait of any mechanism fails;
//! * **the anti-spin half is intact** — a process parked on a finite `receive … after T`
//!   with nothing else to run *must* be waited to, so the same run must spend about `T`
//!   blocked and must do so in a bounded number of syscalls (a busy-spin spends ≈0, a
//!   1 ms polling spin spends ≈`T`/1 ms syscalls).
//!
//! **The mechanism is environment-dependent, so it is not pinned.** Which syscall a wait
//! lands in depends on what else the runtime holds: every CLI action but `-version` drives
//! the boot script, and the booted kernel keeps the `inet_gethost` port open, so on this host
//! the poller always has a live registration and a pass's wait is spent inside
//! `epoll_wait`'s timeout — while on a runtime with no live port the identical pass reaches
//! `std::thread::sleep` (`clock_nanosleep`). [`parse_blocking_waits`] therefore counts **both**
//! (and only when they could block: a zero-timeout readiness probe is not a wait), and the
//! assertions are about the count and the blocked time, never about which syscall produced
//! them.
//!
//! The ping-pong fixture (`tests/fixtures/pingpong.beam`, source + regeneration recipe in
//! `tests/fixtures/README.md`) is a committed oracle fixture: the value it prints is the
//! same under the system `erl`, compared below whenever a toolchain is present.
//!
//! The syscall half needs `strace`; it is skipped **loudly** (never silently passed) when
//! the tool is missing, like the other tool-driven suites in this crate.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// The small round-trip count of the **scaling** comparison: a few passes' worth of work.
const PINGPONG_SMALL_ROUNDS: usize = 2_000;

/// Round trips for the syscall sweep: 10× [`PINGPONG_SMALL_ROUNDS`], so a wait charged per
/// pass (or per round trip) is unmistakable in the ratio while the run stays inside the probe
/// bound.
const PINGPONG_ROUNDS: usize = 20_000;

/// How many more blocking waits the 10×-larger run may issue than the small one.
///
/// The busy run's own count must be **constant**: what it may vary by is the fixed prologue
/// every action pays (the boot script's parked kernel processes), so a handful of syscalls of
/// slack keeps the comparison about scaling rather than about a deterministic zero.
const WAIT_SLACK: usize = 4;

/// The ceiling on the **blocked time** of the busy run: its occasional pass boundary is a
/// 25 ms poll slice at worst, so a fixed handful is expected — a run that sat in one long
/// wait (a `TimerWait::Deadline` leak, say) is not.
const BUSY_MAX_BLOCKED: Duration = Duration::from_millis(1_000);

/// The timeout of the parked receive the anti-spin half drives (`recv_after:after2000/0`).
///
/// Deliberately **not** `after20/0`: the pass waits in 25 ms poll slices, so a 20 ms deadline
/// is overshot by the first slice and costs a single wait that is indistinguishable from the
/// boot prologue's. Two seconds makes "it waited for about its timeout" and "it did so in a
/// bounded number of syscalls" both measurable without a baseline run.
const IDLE_TIMEOUT: Duration = Duration::from_millis(2_000);

/// The least blocked time the parked run must show — comfortably below [`IDLE_TIMEOUT`]
/// (a loaded host may wake a pass late) and far above what a busy-spin would show.
const IDLE_MIN_BLOCKED: Duration = Duration::from_millis(1_500);

/// The ceiling on the parked run's blocking waits: they must be **proportional to the passes,
/// not to the wall time**.
///
/// A pass sleeps a [`PORT_POLL_SLICE`](../beam_rt/port/index.html) (25 ms) at most and is
/// resumed by the timeout, so ≈`IDLE_TIMEOUT`/25 ms ≈ 80 waits is the expected order (and the
/// measure on this host); a pass that polled every millisecond would be ≈2 000, and a
/// busy-spin would be ≈0 — caught by [`IDLE_MIN_BLOCKED`].
const IDLE_WAIT_BOUND: usize = 256;

/// The wait mechanisms traced: the runtime's only two ways to block a pass.
///
/// `clock_nanosleep` is what `std::thread::sleep` issues on Linux ([`Runtime::timer_wait`]'s
/// sleep), and `epoll_wait` is what the runtime's one `beam_io::Poller` issues (`mio`'s linux
/// backend). `nanosleep` is traced too because it is the same call on older libcs.
///
/// `poll`/`ppoll` are deliberately **not** traced: a `poll` line cannot be attributed to the
/// scheduler — the `inet_gethost` port child the booted kernel keeps open is a `fork` of this
/// very process and polls its own pipe with a 5 s timeout, so tracing `poll` would measure the
/// child's I/O loop rather than the pass's wait.
const TRACED_WAITS: &str = "clock_nanosleep,nanosleep,epoll_wait";

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

/// What one straced run did with its time: the blocking waits it issued, and how long it spent
/// inside them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BlockingWaits {
    /// How many traced calls could block (a non-zero timeout).
    count: usize,
    /// The time spent inside them, summed from `strace -T`'s `<0.025280>` fields.
    blocked: Duration,
}

/// Parses an `strace -T` log of the [traced waits](TRACED_WAITS) into the **blocking** subset.
///
/// A call counts only when it *could* block: an `epoll_wait` whose timeout argument is `0` is
/// a readiness probe (the pass issues one per round), not a wait. An `unfinished`/`resumed`
/// pair is counted once — on the resumed line, which is the one `strace` puts the duration on.
fn parse_blocking_waits(log: &str) -> BlockingWaits {
    let mut waits = BlockingWaits::default();
    for line in log.lines() {
        let blocking = if line.contains("clock_nanosleep(") || line.contains("nanosleep(") {
            // A zero-duration sleep is never issued: both `std::thread::sleep`'s caller here
            // (`Runtime::timer_wait`) and the choke point above it skip an empty wait.
            true
        } else if line.contains("epoll_wait") {
            epoll_timeout_ms(line).is_some_and(|ms| ms > 0)
        } else {
            false
        };
        if !blocking {
            continue;
        }
        waits.count += 1;
        waits.blocked += line_duration(line).unwrap_or_default();
    }
    waits
}

/// The timeout argument of an `epoll_wait` line, in milliseconds — the last argument before
/// the argument list's closing `)`, which is spelled the same on a complete call
/// (`epoll_wait(3, [], 1024, 25) = 0`) and on the `resumed` half of an interrupted one
/// (`… epoll_wait resumed>, [], 1024, 25) = 1`).
///
/// The result separator is **not** used to find the argument list: `strace` pads it with the
/// column alignment of the line, so `) =` and `)      =` both occur.
fn epoll_timeout_ms(line: &str) -> Option<u64> {
    let args = line.get(..line.rfind(')')?)?;
    args.rsplit(',').next()?.trim().parse().ok()
}

/// The `strace -T` duration `strace` appends to a line, as `<0.025280>`.
fn line_duration(line: &str) -> Option<Duration> {
    let rest = line.get(line.rfind('<')? + 1..)?;
    let field = rest.get(..rest.find('>')?)?;
    let secs: f64 = field.trim().parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// Runs `binary()` under `strace` on the [traced waits](TRACED_WAITS) and returns what the run
/// did with its time — the call's own process tree (`-f`), so the runtime's single-threaded
/// scheduler and any thread it starts are both covered.
fn straced_blocking_waits(strace: &Path, tag: &str, args: &[&str]) -> (BlockingWaits, String) {
    let trace = scratch_trace_path(tag);
    let trace_arg = trace.to_string_lossy().to_string();
    let mut argv: Vec<&str> = vec!["-f", "-T", "-e", TRACED_WAITS, "-o", &trace_arg, BINARY];
    argv.extend_from_slice(args);
    let output =
        run_bounded(strace, &argv).expect("the straced run finishes within the probe bound");
    let log = std::fs::read_to_string(&trace).expect("strace wrote its log");
    let _ = std::fs::remove_file(&trace);
    assert!(
        output.status.success(),
        "the straced run route failed: {output:?}\n{log}"
    );
    (parse_blocking_waits(&log), log)
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

/// **The wait accounting**: the busy ping-pong pays no wait that scales with its work, and a
/// process parked on a finite `receive … after 2000` is still waited to — without a spin.
///
/// Both halves measure `strace`'s blocking-wait count and blocked time rather than a wall
/// clock, and neither pins the syscall that carries the wait (see the module docs: whether a
/// pass blocks in `epoll_wait` or in `clock_nanosleep` depends on what the booted kernel left
/// live, not on the policy under test).
#[test]
fn the_run_route_never_waits_while_it_has_work_and_still_waits_to_a_receive_timeout() {
    let Some(strace) = find_on_path("strace") else {
        skip_loudly(
            "run_route_sleep",
            "no `strace` on $PATH (the syscall half needs it)",
        );
        return;
    };
    assert!(is_executable(&strace));

    // 1. The busy half: the run queue never empties, so no pass may pay a blocking wait. The
    //    count is measured at two sizes 10× apart — a wait charged per pass (or per round
    //    trip) is visible in the ratio even when the absolute count is small on a fast host.
    let small_rounds = PINGPONG_SMALL_ROUNDS.to_string();
    let large_rounds = PINGPONG_ROUNDS.to_string();
    let (small, small_log) = straced_blocking_waits(
        &strace,
        "pingpong_small",
        &["run", PINGPONG_BEAM, "pingpong:go/1", &small_rounds],
    );
    let (large, large_log) = straced_blocking_waits(
        &strace,
        "pingpong_large",
        &["run", PINGPONG_BEAM, "pingpong:go/1", &large_rounds],
    );
    assert!(
        large.count <= small.count + WAIT_SLACK,
        "the run route has work to do at every pass boundary and must not block for it: \
         {PINGPONG_SMALL_ROUNDS} round trips issued {} blocking wait(s) and {PINGPONG_ROUNDS} \
         issued {} — a wait charged per pass would scale with the work.\n\
         small:\n{small_log}\nlarge:\n{large_log}",
        small.count,
        large.count
    );
    assert!(
        large.count < PINGPONG_ROUNDS / 100,
        "a busy run must not block per unit of work: {} blocking wait(s) for {} round trips\n\
         {large_log}",
        large.count,
        PINGPONG_ROUNDS
    );
    assert!(
        large.blocked <= BUSY_MAX_BLOCKED,
        "a busy run must not sit in a long wait: {} ms blocked across {} wait(s)\n{large_log}",
        large.blocked.as_millis(),
        large.count
    );

    // 2. The anti-spin half: a parked finite `receive … after T` with nothing to run is
    //    **waited to** — it must block for about its timeout and must do so in a bounded
    //    number of syscalls (a busy-spin blocks ≈0; a 1 ms polling spin issues ≈T/1 ms).
    let output = run_bounded(
        binary(),
        &["run", RECV_AFTER_BEAM, "recv_after:after2000/0"],
    )
    .expect("the receive-after fixture finishes within the probe bound");
    assert!(
        output.status.success(),
        "the receive-after run failed: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "timeout",
        "`after 2000` on an empty mailbox times out"
    );
    let (idle, idle_log) = straced_blocking_waits(
        &strace,
        "recv_after",
        &["run", RECV_AFTER_BEAM, "recv_after:after2000/0"],
    );
    assert!(
        idle.blocked >= IDLE_MIN_BLOCKED,
        "a process parked on a finite receive must be waited to, not spun on: {} ms blocked \
         across {} wait(s), for a {} ms timeout\n{idle_log}",
        idle.blocked.as_millis(),
        idle.count,
        IDLE_TIMEOUT.as_millis()
    );
    assert!(
        idle.count <= IDLE_WAIT_BOUND,
        "a parked receive must not poll-spin: {} blocking wait(s) for a {} ms park \
         (the bound is {IDLE_WAIT_BOUND} — the count grows with the passes, never with the \
         wall time)\n{idle_log}",
        idle.count,
        IDLE_TIMEOUT.as_millis()
    );
}
