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

//! `erlang:spawn_monitor/1,2,3,4` at the **executable** boundary (contracts A.7/A.9).
//!
//! `erlang.erl` implements the family as wrappers over `erlang:spawn_opt(…, [monitor])`, so it
//! is not part of the `bif.tab` surface: it is served by the runtime-native BIF floor
//! (`src/native/process.rs`), which the scheduler path reaches before the registry. This suite
//! pins the path a user sees — a real `.beam` compiled at test time, entered as a **scheduled
//! process** through the built `beam_rt` binary (`run <file> spawnmon:<fn>/0`) — and compares
//! the printed answer line for line with the live system `erl` oracle running the *same*
//! module.
//!
//! # What the flows prove
//!
//! * **The monitor is established before the child's first schedulable moment.** `run/0`'s
//!   child is `fun() -> ok end`, i.e. it exits immediately; the `{'DOWN', Ref, process, Pid,
//!   normal}` still arrives, which is only possible if the monitor was on record before the
//!   child could run (a child that ran and died first would leave the monitor waiting for a
//!   process that is already gone).
//! * **Every arity works against this runtime's own node.** `local_node/0` spawns through
//!   `/2` (a 0-arity fun), `/3` (a module/function child) and `/4` (the node *and* the
//!   module/function child), each answering `{Pid, Ref}` and delivering its `DOWN`.
//! * **The guard rejections are the oracle's `badarg`**, never a fabricated child: a
//!   wrong-arity fun (`/1`), a non-atom node (`/2`, `/4`) and a non-list argument list (`/3`).
//! * **The remote-node form is the typed capability gap** (contract A.5), asserted separately
//!   below because the oracle answers it through a distributed `erlang:spawn_request/5`.
//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`) — re-probed, not guessed
//!
//! The fixture is compiled into a scratch directory with `erlc` run **with the scratch
//! directory as cwd** (so nothing path-dependent is committed), and that directory never
//! contains a `lists.beam`, so it is safe to hand to the oracle as a single `-pa` (unlike this
//! crate's `tests/fixtures/`). The three cross-checked flows print exactly:
//!
//! ```text
//! {{pid,true,true,normal},badarg}    %% run/0          — the two cases the suite pins
//! {true,true,true}                   %% local_node/0   — /2, /3 and /4 all deliver their DOWN
//! [badarg,badarg,badarg,badarg,badarg]  %% badargs/0    — every guard rejection
//! ```
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery,
// its loud skip, the oracle probe and the probe bound, so the unused remainder (the `-eval`
// exit-code helper, the `badarg` assertion) is expected here.
#[allow(dead_code)]
mod support;

/// The committed fixture source (compiled at test time — never committed as a `.beam`).
const FIXTURE_ERL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/spawnmon.erl");

/// The fixture's module name.
const MODULE: &str = "spawnmon";

/// One fixture entry point, described by what it proves and the exact line **both** runtimes
/// print for it (re-probed against the live oracle, then asserted by
/// [`every_spawn_monitor_flow_matches_the_live_oracle`]).
const FLOWS: [(&str, &str, &str); 3] = [
    (
        "a child that exits immediately still delivers its DOWN, and a wrong-arity fun is badarg",
        "run",
        "{{pid,true,true,normal},badarg}",
    ),
    (
        "every node-taking arity on this runtime's own node answers {Pid,Ref} and delivers its DOWN",
        "local_node",
        "{true,true,true}",
    ),
    (
        "every guard-failing form is the oracle's badarg, and no child is created",
        "badargs",
        "[badarg,badarg,badarg,badarg,badarg]",
    ),
];

/// The node forms that are this runtime's typed capability gap, with the arity each reports in
/// its message. `erlang.erl`'s local-node clause is `N =:= erlang:node()`; any other atom node
/// falls to the distributed `erlang:spawn_request/5` clause (contract A.5), which this runtime
/// has no send path for — so it answers a typed gap rather than a fabricated child.
const GAPS: [(&str, u32); 2] = [("remote", 2), ("remote4", 4)];

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
///
/// `$ERTS_ORACLE`/`$PATH` discovery is `tests/support/mod.rs`'s (filesystem only — a probe
/// must never *run* `erl` to find out whether it exists).
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

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding `spawnmon.erl` and `spawnmon.beam`.
    dir: PathBuf,
    /// The compiled `spawnmon.beam`.
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

/// Compiles the committed `spawnmon.erl` into a scratch directory, or `None` when the system
/// `erlc` is unavailable (the caller then returns early — **loudly**).
///
/// The source is copied to `<scratch>/spawnmon.erl` and compiled with `erlc` run **with the
/// scratch directory as cwd** and the bare argument, exactly as `tests/position.rs` and
/// `tests/spawn_from_slice.rs` do, so the module is self-contained and the scratch dir is a
/// safe single `-pa` for the oracle.
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
    if let Err(error) = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join("spawnmon.erl"), &source))
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
        .arg("spawnmon.erl")
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
    let beam = dir.join("spawnmon.beam");
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
/// The fixture's `receive`s all carry a 5 s `after` clause and `beam_rt run` is itself bounded
/// by a wall-clock deadline, so every probe here terminates on its own; the polling guard is
/// the belt to that suspenders (`tests/exit.rs` and `tests/spawn_from_slice.rs` keep the same
/// local runner for the same reason).
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
                    "`{}` outlived the {}s probe bound",
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
        &["run", beam, &format!("{MODULE}:{function}/0")],
    )
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every `spawn_monitor` flow this runtime serves must print exactly the line the live oracle
/// prints for the same function of the same module: an immediately-exiting monitored child
/// still delivers its `DOWN`, every node-taking arity works against this runtime's own node,
/// and every guard rejection is the oracle's `badarg` (never the `unsupported` signal, never a
/// wrong value).
#[test]
fn every_spawn_monitor_flow_matches_the_live_oracle() {
    let Some(oracle) = support::require_oracle("spawn_monitor") else {
        return;
    };
    let Some(fixture) = build_fixture("spawn_monitor") else {
        return;
    };

    for (label, function, expected) in FLOWS {
        let ours = run_fixture(&fixture, function);
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt run {function}` exited {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: spawn_monitor is not a capability gap: {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

        let expr = format!("io:format(\"~p~n\", [{MODULE}:{function}()]), halt(0).");
        let theirs = support::oracle_eval(&oracle, Some(&fixture.dir), &expr)
            .unwrap_or_else(|| panic!("{label}: the live oracle could not answer"));
        assert_eq!(
            theirs.lines().next().unwrap_or_default(),
            expected,
            "{label}: the recorded answer must be the live oracle's"
        );
    }
}

/// The node forms name a node that is **not** this one, so the oracle's answer goes through a
/// distributed `erlang:spawn_request/5` (contract A.5) and this runtime answers the typed
/// capability gap instead: exit code 3 with `beam_rt: unsupported: …`, no fabricated child and
/// nothing on stdout.
///
/// `// documented divergence:` there is no oracle cross-check here, and none is possible — a
/// peer-less oracle does *not* reject the call: it answers `{Pid, Ref}` for the unreachable
/// name and logs a `Can not start … on other@nohost` warning report, i.e. it reports a
/// reference to a process it could not create, which this runtime will not fabricate.
#[test]
fn a_remote_node_is_the_typed_capability_gap() {
    let Some(fixture) = build_fixture("spawn_monitor_remote") else {
        return;
    };

    for (function, arity) in GAPS {
        let ours = run_fixture(&fixture, function);
        assert_eq!(
            ours.code, 3,
            "{function}: expected the capability-gap exit code, got {}: {}",
            ours.code, ours.err
        );
        assert_eq!(
            ours.err,
            format!(
                "beam_rt: unsupported: erlang:spawn_monitor/{arity} on a node other than this one"
            ),
            "{function}: the typed capability gap must name the call"
        );
        assert!(
            ours.out.is_empty(),
            "{function}: a gap prints nothing on stdout, got {:?}",
            ours.out
        );
    }
}
