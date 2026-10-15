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

//! `erlang:spawn`/`spawn_link`/`spawn_opt` **from inside a scheduler slice**.
//!
//! `beam_rt run <file>` enters the workload as an ordinary **scheduled process**, so every
//! code path the fixture runs executes inside a scheduler pass. A pass *moves* the runtime's
//! process table and runner state out for its duration (the runtime keeps an empty
//! placeholder), so a spawn made there can reach neither the runtime's own table nor the
//! pass's run queue through `Runtime::spawn_process`. `src/native/process.rs` therefore
//! creates such a child in the **pass's** table and `Runtime::absorb_pending_spawns`
//! registers its engine in the pass's runner state and enqueues it — this suite is the
//! end-to-end proof that the child really runs, for every spawn form:
//!
//! * a fun-bodied `spawn/1`;
//! * an export-bodied `spawn/3`;
//! * `spawn_link/1`;
//! * `spawn_opt/2` with `monitor` (the child's `{'DOWN', _, process, _, normal}`);
//! * a **nested** spawn (a child spawning its own child, from *its* slice);
//! * the pass table really holds the child (`erlang:processes/0` seen from inside the slice);
//! * a **rejected** option list leaves no child behind.
//!
//! The fixture (`tests/fixtures/spawnslice.erl`) is compiled **at test time** with the system
//! `erlc`, so nothing path-dependent is committed, and the same module is run under the live
//! system `erl` oracle — the two runtimes must print exactly the same line. A missing
//! `erl`/`erlc` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).
//!
//! The scratch directory never contains a `lists.beam`, so it is safe to hand to the oracle as
//! a single `-pa` (unlike this crate's `tests/fixtures/`).
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

mod support;

/// The committed fixture source (compiled at test time — never committed as a `.beam`).
const FIXTURE_ERL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/spawnslice.erl");

/// The fixture's module name.
const MODULE: &str = "spawnslice";

/// One fixture entry point, described by what it proves and the exact line **both** runtimes
/// print for it (re-probed against the live oracle, then asserted by
/// [`spawning_from_a_scheduler_slice_reaches_the_children`]).
const FLOWS: [(&str, &str, &str); 3] = [
    (
        "every spawn form reaches its child",
        "run",
        "[true,true,true,true,true,true]",
    ),
    (
        "the child lands in the pass table and runs",
        "tracked",
        "{true,true}",
    ),
    (
        "a rejected spawn_opt option leaves no child behind",
        "rejected",
        "{badarg,true}",
    ),
];

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

/// Prints the loud skip banner — a missing compiler is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt spawn-from-slice SKIPPED ===============");
    eprintln!("{why}: the spawn-from-a-scheduler-slice proof cannot run.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("===============================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding `spawnslice.erl` and `spawnslice.beam`.
    dir: PathBuf,
    /// The compiled `spawnslice.beam`.
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

/// Compiles the committed `spawnslice.erl` into a scratch directory, or `None` when it cannot
/// be compiled (the caller then returns early — **loudly**).
///
/// The source is copied to `<scratch>/spawnslice.erl` and compiled with `erlc` run **with the
/// scratch directory as cwd** and the bare argument, exactly as `tests/position.rs` does, so
/// the module is self-contained and the scratch dir is a safe single `-pa` for the oracle.
fn build_fixture(erlc: &Path) -> Option<Fixture> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_spawnslice_{}_{n}",
        std::process::id()
    ));
    let source = match std::fs::read(FIXTURE_ERL) {
        Ok(bytes) => bytes,
        Err(error) => {
            skip_loudly(&format!("cannot read {FIXTURE_ERL}: {error}"));
            return None;
        }
    };
    if let Err(error) =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join("spawnslice.erl"), &source))
    {
        skip_loudly(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg("spawnslice.erl")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            skip_loudly(&format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip_loudly(&format!(
            "`erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join("spawnslice.beam");
    if !beam.is_file() {
        skip_loudly(&format!("`erlc` produced no {}", beam.display()));
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
/// the belt to that suspenders (`tests/exit.rs` keeps the same local runner for the same
/// reason).
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
            Ok(Some(_)) => break child
                .wait_with_output()
                .unwrap_or_else(|error| panic!("cannot collect the probe's output: {error}")),
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

// ---------------------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------------------

/// `beam_rt run <fixture>.beam <module>:<function>/0` must print exactly the line the live
/// oracle prints for the same function of the same module: every spawn form's child really
/// runs, the child is visible in the pass table, and a rejected option leaves nothing behind.
#[test]
fn spawning_from_a_scheduler_slice_reaches_the_children() {
    let Some(oracle) = support::require_oracle("spawn_from_slice") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixture) = build_fixture(&erlc) else {
        return;
    };
    let beam = fixture
        .beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", fixture.beam.display()));

    for (label, function, expected) in FLOWS {
        let spec = format!("{MODULE}:{function}/0");
        let ours = run_bounded(
            Path::new(env!("CARGO_BIN_EXE_beam_rt")),
            // `-noshell`: the `run` action is banner-free by construction (it records the
            // `noshell` init argument, so the booted kernel prints no OTP slogan); it is kept
            // to mirror the oracle probe.
            &["-noshell", "run", beam, &spec],
        );
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt run {beam} {spec}` exited {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: a spawn from inside a scheduler slice is not a capability gap: {}",
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
