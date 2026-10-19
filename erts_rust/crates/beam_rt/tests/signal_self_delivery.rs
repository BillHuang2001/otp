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

//! A signal a process queues on **itself** must be visible to the `receive` that follows it in
//! the SAME reduction slice — at the **executable** boundary (contracts A.7/A.8).
//!
//! Every shape in `tests/fixtures/sigself.erl` signals a monitored or linked `P` and then
//! receives the resulting `{'DOWN', …}`/`{'EXIT', …}` with no intervening blocking call, so the
//! receiver is still `Running` when the signal is queued. The oracle materialises it into the
//! mailbox inside the receive scan (`i_loop_rec` → `erts_proc_sig_receive_helper` →
//! `erts_proc_sig_fetch`), and a signal no scan can apply to a running receiver re-queues the
//! process at schedule-out instead of letting it sleep. This suite pins both halves through the
//! built `beam_rt` binary (`run <file> sigself:<fn>/0`) against the live system `erl` oracle
//! running the *same* module, comparing the printed line and the exit code.
//!
//! # What the flows prove
//!
//! * **The `'DOWN'` is fetched inside the scan.** `killed`/`killed_after`/`ran_before_kill`/
//!   `spawn_monitor_kill`/`not_matching` monitor `P` and kill it: a missed signal shows up as the
//!   `after` branch (`timeout`) or, for `bare_receive`, as a block that never ends.
//! * **The schedule-out net covers every other class.** `trapping_link` installs `trap_exit`, so
//!   the queued signal is an **exit** signal (not a DOWN) that only the schedule-in service may
//!   turn into a message; the process must be re-queued rather than parked, or the message never
//!   arrives.
//! * **A drain is not a rewrite of the scan.** `not_matching` first runs a receive whose pattern
//!   cannot match the materialised `'DOWN'` (and times out at `after 0`) and only then receives
//!   it: the signal must reach the mailbox, not be consumed by the first scan.
//! * **`natural` agrees too** — a DOWN that arrives by the ordinary send/delivery route keeps
//!   answering `normal`, so the fix cannot have been "always deliver something".
//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`) — re-probed, never guessed
//!
//! The fixture is compiled into a per-run scratch directory with `erlc` run **with that directory
//! as cwd** (so nothing path-dependent is committed), and the directory never contains a
//! `lists.beam`, so it is safe to hand to the oracle as a single `-pa` (unlike this crate's
//! `tests/fixtures/`). The oracle's own answers are re-derived here at run time — every expected
//! string below comes from a live probe of the same function, and `all/0` must print exactly
//! `[{killed,killed},{natural,normal},{killed_after,killed},{ran_before_kill,killed},`
//! `{spawn_monitor_kill,killed},{trapping_link,killed},{bare_receive,killed},{not_matching,killed}]`
//! under both runtimes.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery,
// its loud skip, the oracle probe and the probe bound, so the unused remainder (the `-eval`
// exit-code helper, the `badarg` assertion, the GC payload check) is expected here.
#[allow(dead_code)]
mod support;

/// The committed fixture source (compiled at test time — never committed as a `.beam`).
const FIXTURE_ERL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sigself.erl");

/// The fixture's module name.
const MODULE: &str = "sigself";

/// The eight entry points, each described by the shape it exercises. The expected line is
/// re-derived from the live oracle for every one of them (see
/// [`every_self_signalled_shape_prints_the_live_oracles_own_line`]).
const SHAPES: [(&str, &str); 8] = [
    (
        "a killed monitored child's DOWN matches in a receive with an `after` clause",
        "killed",
    ),
    (
        "a natural death's DOWN still arrives by the ordinary delivery route",
        "natural",
    ),
    (
        "a pre-armed `after` clause shows a missed signal as `timeout` instead of a block",
        "killed_after",
    ),
    (
        "the kill takes the live-blocked-target path once the child has run to its receive",
        "ran_before_kill",
    ),
    (
        "the monitor is armed at spawn (`spawn_monitor`) and its DOWN still matches",
        "spawn_monitor_kill",
    ),
    (
        "a trapping receiver's exit signal becomes a message, not a park",
        "trapping_link",
    ),
    (
        "a `receive` with no `after` clause at all is woken by the process's own signal",
        "bare_receive",
    ),
    (
        "a scan whose pattern cannot match does not consume the queued DOWN",
        "not_matching",
    ),
];

/// `all/0` drives every shape in one call, in the order the suite prints them.
const ALL: &str = "all";

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
///
/// `$ERTS_ORACLE`/`$PATH` discovery is `tests/support/mod.rs`'s (filesystem only — a probe must
/// never *run* `erl` to find out whether it exists).
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// The compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding `sigself.erl` and `sigself.beam`.
    dir: PathBuf,
    /// The compiled `sigself.beam`.
    beam: PathBuf,
}

impl Drop for Fixture {
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

/// Compiles the committed `sigself.erl` into a per-run scratch directory, or `None` when the
/// system `erlc` is unavailable (the caller then returns early — **loudly**).
///
/// The source is copied to `<scratch>/sigself.erl` and compiled with `erlc` run **with the
/// scratch directory as cwd** and the bare argument, exactly as `tests/spawn_monitor.rs` does,
/// so the module is self-contained and the scratch dir is a safe single `-pa` for the oracle.
fn build_fixture(banner: &str) -> Option<Fixture> {
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(banner, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_{banner}_{}_{n}", std::process::id()));
    let source = match std::fs::read(FIXTURE_ERL) {
        Ok(bytes) => bytes,
        Err(error) => {
            support::skip_loudly(banner, &format!("cannot read {FIXTURE_ERL}: {error}"));
            return None;
        }
    };
    if let Err(error) =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join("sigself.erl"), &source))
    {
        support::skip_loudly(
            banner,
            &format!("cannot prepare {}: {error}", dir.display()),
        );
        return None;
    }
    let output = match Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg("sigself.erl")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(banner, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            banner,
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let beam = dir.join("sigself.beam");
    if !beam.is_file() {
        support::skip_loudly(banner, &format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixture { dir, beam })
}

// ---------------------------------------------------------------------------------------
// The bounded probe
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed and
/// reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite.
///
/// This is what makes the *bare* receive shape provably non-hanging: `sigself:bare_receive/0`
/// has no `after` clause, so a lost signal parks the process forever and `beam_rt run`'s own
/// deadline is what ends it. The poll below turns "still running at the bound" into a **test
/// failure**, never a hung suite.
fn run_bounded(program: &Path, args: &[&str]) -> Run {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn `{}`: {error}", program.display()));
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    let output: Output = loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                break child
                    .wait_with_output()
                    .unwrap_or_else(|error| panic!("cannot collect the probe's output: {error}"))
            }
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "`{}` outlived the {}s probe bound — the signal was never delivered",
                    program.display(),
                    support::PROBE_TIMEOUT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot poll `{}`: {error}", program.display()),
        }
    };
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt run <fixture.beam> <module>:<function>/0` through the built binary.
fn run_fixture(fixture: &Fixture, function: &str) -> Run {
    let beam = fixture
        .beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", fixture.beam.display()));
    run_bounded(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        // `-noshell`: the `run` action is banner-free by construction (it records the `noshell`
        // init argument, so the booted kernel prints no OTP slogan) and prints only its own
        // `~w~n` line — so a capability gap still prints *nothing* on stdout. Kept to mirror the
        // oracle probe.
        &["-noshell", "run", beam, &format!("{MODULE}:{function}/0")],
    )
}

/// The oracle's own line for `sigself:<function>/0`, re-derived from the live system `erl`.
///
/// `~w` (not `~p`) so the answer is **flat**: the two runtimes print the same bytes, which is
/// what lets a multi-element list be compared line for line. `None` when the probe could not
/// answer (the caller reports a loud skip).
fn oracle_line(oracle: &Path, fixture: &Fixture, function: &str) -> Option<String> {
    let expr = format!("io:format(\"~w~n\", [{MODULE}:{function}()]), halt(0).");
    support::oracle_eval(oracle, Some(&fixture.dir), &expr)
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every shape must print exactly the line the live oracle prints for the same function of the
/// same module, with exit code 0 — the signal the process queued on itself is delivered before
/// the `receive` blocks, whether it is a `'DOWN'` (fetched inside the scan) or an exit signal (a
/// trapping receiver's message, which needs the process to be re-queued rather than parked).
#[test]
fn every_self_signalled_shape_prints_the_live_oracles_own_line() {
    let Some(oracle) = support::require_oracle("signal_self_delivery") else {
        return;
    };
    let Some(fixture) = build_fixture("signal_self_shapes") else {
        return;
    };

    for (label, function) in SHAPES {
        let theirs = oracle_line(&oracle, &fixture, function).unwrap_or_else(|| {
            panic!("{label} (`{function}`): the live oracle could not answer")
        });

        let ours = run_fixture(&fixture, function);
        assert_eq!(
            ours.code, 0,
            "{label} (`{function}`): `beam_rt run` exited {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label} (`{function}`): a self-signalled receive is not a capability gap: {}",
            ours.err
        );
        assert_eq!(
            ours.out, theirs,
            "{label} (`{function}`): beam_rt's answer must be the oracle's"
        );
    }
}

/// `sigself:all/0` drives all eight shapes in one call: both runtimes must print the oracle's
/// eight-element list, element order included, and both must exit 0.
///
/// The expected list is the live oracle's, never a hardcoded string; each element is also
/// checked against the same shape's individual answer so the two tests cannot disagree.
#[test]
fn all_prints_the_oracles_eight_element_list() {
    let Some(oracle) = support::require_oracle("signal_self_delivery") else {
        return;
    };
    let Some(fixture) = build_fixture("signal_self_all") else {
        return;
    };

    let theirs = oracle_line(&oracle, &fixture, ALL)
        .unwrap_or_else(|| panic!("the live oracle could not answer `{ALL}/0`"));
    assert!(
        theirs.starts_with('[') && theirs.ends_with(']'),
        "the oracle's `{ALL}/0` answer is the eight-element list, got {theirs:?}"
    );

    let ours = run_fixture(&fixture, ALL);
    assert_eq!(
        ours.code, 0,
        "`beam_rt run {ALL}/0` exited {}: {}",
        ours.code, ours.err
    );
    assert_eq!(
        ours.out, theirs,
        "`{ALL}/0` must print the oracle's list, in the oracle's order"
    );

    // Every shape's own answer is the one the list carries for it (`{Function, Answer}`), on
    // both runtimes — so the per-shape and whole-list halves of this suite agree.
    for (label, function) in SHAPES {
        let single = oracle_line(&oracle, &fixture, function)
            .unwrap_or_else(|| panic!("{label} (`{function}`): the live oracle could not answer"));
        let element = format!("{{{function},{single}}}");
        assert!(
            theirs.contains(&element),
            "{label}: the oracle's `{ALL}/0` list must carry `{element}`: {theirs}"
        );
        assert!(
            ours.out.contains(&element),
            "{label}: beam_rt's `{ALL}/0` list must carry `{element}`: {}",
            ours.out
        );
    }
}

/// The **bare** receive — no `after` clause at all — is the shape a missed signal turns into an
/// endless park, so it is asserted on its own: the probe is bounded
/// ([`support::PROBE_TIMEOUT`]) and a process that is still running at the bound **fails** this
/// test instead of hanging the suite.
#[test]
fn a_bare_receive_with_no_after_clause_is_woken_by_its_own_signal() {
    let Some(oracle) = support::require_oracle("signal_self_delivery") else {
        return;
    };
    let Some(fixture) = build_fixture("signal_self_bare") else {
        return;
    };

    let theirs = oracle_line(&oracle, &fixture, "bare_receive")
        .unwrap_or_else(|| panic!("the live oracle could not answer `bare_receive/0`"));

    let ours = run_fixture(&fixture, "bare_receive");
    assert_eq!(
        ours.code, 0,
        "`beam_rt run bare_receive/0` exited {} (a bound hit here means the receive never woke): \
         {}",
        ours.code, ours.err
    );
    assert_eq!(
        ours.out, theirs,
        "a `receive` with no `after` clause must still be woken by the process's own `'DOWN'`"
    );
}
