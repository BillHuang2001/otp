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

//! Contract A.6/A.8 — **the action's own process identity**, cross-checked against the
//! system Erlang oracle.
//!
//! # What this suite pins
//!
//! The oracle runs a CLI action in a process `init` *spawns*, not in `init` itself. Probed
//! live (`OTP 29.1.1 / erts-17.1`),
//! `erl -noshell -eval 'process_info(self(), registered_name)'` answers `[]` while
//! `whereis(init)` answers the root pid, so inside the action `whereis(init) =:= self()` is
//! `false` and `self()` is a pid **no name is registered for**. An earlier `beam_rt` ran
//! `-eval`/`-s`/`-run` in the process it registers as `init`, so the action answered
//! `{registered_name,init}` and `whereis(init) =:= self()` was `true` — an observable
//! divergence on the caller's identity.
//!
//! `Runtime::as_action_process` (see `src/runtime.rs`) moves the action onto a freshly spawned,
//! **unregistered** child for the duration of the body, and restores the previous identity on
//! every exit path. This suite is the compatibility gate for that:
//!
//! * `the_eval_action_identity_matches_the_oracle` — the `-eval` route, the same expression
//!   under both runtimes.
//! * `the_s_action_identity_matches_the_oracle` / `the_run_action_identity_matches_the_oracle`
//!   — `-s`/`-run`, which **discard** their result, so the action itself has to print what it
//!   sees: `tests/fixtures/action_identity.erl` is compiled at test time with the system
//!   `erlc` and driven under both runtimes.
//! * `the_init_process_identity_survives_the_action_routes` — the **preserved** half: only the
//!   action's *own* identity moves; `whereis(init)`, `process_info(init, registered_name)`,
//!   `init:get_status/0` and `application:which_applications/0` must keep answering what they
//!   answered before.
//!
//! A missing toolchain (`erl`/`erlc`) is a **loud** skip (`support::require_oracle` /
//! `support::skip_loudly`), never a silent pass.
//!
//! # Why the fixture, not `-eval`, observes `-s`/`-run`
//!
//! `-s Mod Func Args…` applies `Mod:Func(Args)` and **discards** the result (`-run` likewise),
//! so the only way to see which process the action ran in is for the action to print it. The
//! fixture does exactly that. `tests/fixtures/` itself is **never** passed to the oracle as a
//! `-pa`: it holds a `lists.beam` that would shadow stdlib — only the fixture's own scratch
//! output directory is put on each runtime's code path.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    /// The finished probe described by one collected child `output`.
    fn from_output(output: Output) -> Run {
        Run {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
    }
}

/// Runs `program args` under the shared bounded runner (`/dev/null` stdin, killed at
/// `support::PROBE_TIMEOUT`), failing loudly when the probe could not be run at all.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Run::from_output(output)
}

/// `beam_rt <args>` through the built binary.
fn beam_rt(args: &[&str]) -> Run {
    run(Path::new(BINARY), args)
}

// ---------------------------------------------------------------------------------------
// Temporary directories and the fixture
// ---------------------------------------------------------------------------------------

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_action_identity_<pid>_<n>_<label>`, unique per test and per run.
    fn new(label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_action_identity_{}_{unique}_{label}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The compiled fixture module, in its own scratch directory.
struct Fixture {
    /// The scratch output directory, the only `-pa` either runtime is given.
    outdir: TempDir,
}

impl Fixture {
    /// The scratch directory as an `OsStr`-lossy string, for a `-pa` argument.
    fn outdir(&self) -> String {
        self.outdir.0.to_string_lossy().into_owned()
    }
}

/// Compiles `tests/fixtures/action_identity.erl` with the system `erlc` into a private scratch
/// directory, or reports a loud skip when `erlc` is absent.
fn compile_fixture(banner: &str) -> Option<Fixture> {
    let Some(erlc) = support::find_on_path("erlc") else {
        support::skip_loudly(banner, "no `erlc` on $PATH");
        return None;
    };
    let outdir = TempDir::new("fixture");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/action_identity.erl");
    let compiled = run(
        &erlc,
        &[
            "-o",
            &outdir.0.to_string_lossy(),
            &source.to_string_lossy(),
        ],
    );
    assert_eq!(
        compiled.code, 0,
        "erlc must build the action-identity fixture: {}",
        compiled.err
    );
    assert!(
        outdir.0.join("action_identity.beam").is_file(),
        "erlc reported success but wrote no action_identity.beam in {}",
        outdir.0.display()
    );
    Some(Fixture { outdir })
}

// ---------------------------------------------------------------------------------------
// The `-eval` route
// ---------------------------------------------------------------------------------------

/// The four observations, in a fixed order, on one line: `registered_name`, whether `self()`
/// is the `init` pid, whether `self()` is a live pid, and whether `node(self())` is this node.
const IDENTITY_EXPR: &str = "io:format(\"rn=~p eq=~p ispid=~p nodeok=~p~n\",\
     [process_info(self(),registered_name),\
      whereis(init)=:=self(),\
      is_pid(self()),\
      node(self())=:=node()]), halt(0).";

/// The exact line both runtimes must print for [`IDENTITY_EXPR`].
const IDENTITY_LINE: &str = "rn=[] eq=false ispid=true nodeok=true";

/// The `-eval` action must run in a spawned, **unregistered** process: `registered_name` is
/// `[]`, `self()` is not the `init` pid (`whereis(init) =:= self()` is `false`), `self()` is a
/// live pid, and it is on this node.
///
/// The same expression is asked of both runtimes and their stdout and exit code must agree; the
/// runtime's own answer is additionally pinned to [`IDENTITY_LINE`], so the assertion still
/// means something when the oracle is unavailable (which is a loud skip).
#[test]
fn the_eval_action_identity_matches_the_oracle() {
    let ours = beam_rt(&["-noshell", "-eval", IDENTITY_EXPR]);
    assert_eq!(
        ours.code, 0,
        "-eval must succeed: exit {} — {}",
        ours.code, ours.err
    );
    assert!(
        ours.err.is_empty(),
        "-eval must be silent on stderr: {:?}",
        ours.err
    );
    assert_eq!(
        ours.out, IDENTITY_LINE,
        "an -eval action runs in a spawned, unregistered process"
    );

    let Some(oracle) = support::require_oracle("action_identity") else {
        return;
    };
    let theirs = run(
        &oracle,
        &["-noshell", "-eval", IDENTITY_EXPR, "-s", "init", "stop"],
    );
    assert_eq!(
        theirs.code, 0,
        "the oracle must run the same probe: exit {} — {}",
        theirs.code, theirs.err
    );
    assert_eq!(
        ours.out, theirs.out,
        "the -eval action's identity must agree with the oracle"
    );
    eprintln!("-eval action identity agrees with the oracle: {}", ours.out);
}

// ---------------------------------------------------------------------------------------
// The `-s` / `-run` routes
// ---------------------------------------------------------------------------------------

/// `-s` discards its result, so the fixture prints what it sees; both runtimes must print the
/// same line, which is the `-eval` line with the pid-bearing fields dropped (the fixture
/// reports the same four observations minus `self()`'s pid, which necessarily differs).
#[test]
fn the_s_action_identity_matches_the_oracle() {
    let Some(fixture) = compile_fixture("action_identity") else {
        return;
    };
    let outdir = fixture.outdir();

    let ours = beam_rt(&["-noshell", "-pa", &outdir, "-s", "action_identity", "ident"]);
    assert_eq!(
        ours.code, 0,
        "-s must succeed: exit {} — {}",
        ours.code, ours.err
    );
    assert!(
        ours.err.is_empty(),
        "-s must be silent on stderr: {:?}",
        ours.err
    );
    assert_eq!(
        ours.out, IDENTITY_LINE,
        "a -s action runs in a spawned, unregistered process"
    );

    let Some(oracle) = support::require_oracle("action_identity") else {
        return;
    };
    let theirs = run(
        &oracle,
        &[
            "-noshell",
            "-pa",
            &outdir,
            "-s",
            "action_identity",
            "ident",
            "-s",
            "init",
            "stop",
        ],
    );
    assert_eq!(
        theirs.code, 0,
        "the oracle must run the same probe: exit {} — {}",
        theirs.code, theirs.err
    );
    assert_eq!(
        ours.out, theirs.out,
        "the -s action's identity must agree with the oracle"
    );
    eprintln!("-s action identity agrees with the oracle: {}", ours.out);
}

/// The `-run` twin of [`the_s_action_identity_matches_the_oracle`] (the arguments differ in
/// shape, the identity contract does not).
#[test]
fn the_run_action_identity_matches_the_oracle() {
    let Some(fixture) = compile_fixture("action_identity") else {
        return;
    };
    let outdir = fixture.outdir();

    let ours = beam_rt(&["-noshell", "-pa", &outdir, "-run", "action_identity", "ident"]);
    assert_eq!(
        ours.code, 0,
        "-run must succeed: exit {} — {}",
        ours.code, ours.err
    );
    assert!(
        ours.err.is_empty(),
        "-run must be silent on stderr: {:?}",
        ours.err
    );
    assert_eq!(
        ours.out, IDENTITY_LINE,
        "a -run action runs in a spawned, unregistered process"
    );

    let Some(oracle) = support::require_oracle("action_identity") else {
        return;
    };
    let theirs = run(
        &oracle,
        &[
            "-noshell",
            "-pa",
            &outdir,
            "-run",
            "action_identity",
            "ident",
            "-s",
            "init",
            "stop",
        ],
    );
    assert_eq!(
        theirs.code, 0,
        "the oracle must run the same probe: exit {} — {}",
        theirs.code, theirs.err
    );
    assert_eq!(
        ours.out, theirs.out,
        "the -run action's identity must agree with the oracle"
    );
    eprintln!("-run action identity agrees with the oracle: {}", ours.out);
}

// ---------------------------------------------------------------------------------------
// The preserved half: only the action's *own* identity moves
// ---------------------------------------------------------------------------------------

/// The `init` process's view, read from **inside** an `-eval` action: `whereis(init)` is still
/// the root pid (a live pid), it is still registered as `init`, it is not `self()`, the node
/// reports `{starting,started}` and the application view is populated.
///
/// Two short lines rather than one long one, so the comparison never depends on the `~p`
/// printer's line-wrapping width — a different concern from the action's identity.
const PRESERVED_EXPR: &str = "io:format(\"init=~p ispid=~p rn=~p selfeq=~p~n\",\
     [whereis(init),\
      is_pid(whereis(init)),\
      process_info(whereis(init),registered_name),\
      whereis(init)=:=self()]),\
     io:format(\"st=~p apps=~p~n\",\
     [init:get_status(),length(application:which_applications())]),\
     halt(0).";

/// The identity moving onto the action process must not disturb the node's other observable
/// process facts: `whereis(init)`, `is_pid(whereis(init))`,
/// `process_info(whereis(init), registered_name)`, `init:get_status/0` and the application
/// view all keep answering what the oracle answers.
#[test]
fn the_init_process_identity_survives_the_action_routes() {
    let ours = beam_rt(&["-noshell", "-eval", PRESERVED_EXPR]);
    assert_eq!(
        ours.code, 0,
        "-eval must succeed: exit {} — {}",
        ours.code, ours.err
    );
    // `init` stays the root pid, stays registered, and is still *not* the action's pid.
    assert!(
        ours.out.contains("init=<0.0.0>")
            && ours.out.contains("ispid=true")
            && ours.out.contains("rn={registered_name,init}")
            && ours.out.contains("selfeq=false")
            && ours.out.contains("st={starting,started}"),
        "the init process must be untouched by the action's own identity: {:?}",
        ours.out
    );

    let Some(oracle) = support::require_oracle("action_identity") else {
        return;
    };
    let theirs = run(
        &oracle,
        &["-noshell", "-eval", PRESERVED_EXPR, "-s", "init", "stop"],
    );
    assert_eq!(
        theirs.code, 0,
        "the oracle must run the same probe: exit {} — {}",
        theirs.code, theirs.err
    );
    assert_eq!(
        ours.out, theirs.out,
        "the node's process facts must agree with the oracle"
    );
    eprintln!("the preserved process facts agree with the oracle: {}", ours.out);
}
