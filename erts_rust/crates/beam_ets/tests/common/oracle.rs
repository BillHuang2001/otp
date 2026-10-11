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

//! Shared plumbing for the integration tests that shell out to the **system Erlang
//! oracle** (`erl`/`erlc`/`escript` on `$PATH`, or whatever `$ERTS_ORACLE`/`$ERLC`
//! point at).
//!
//! # The oracle gate
//!
//! The oracle cross-checks are the crate's compatibility evidence, so they **run by
//! default and a genuinely absent Erlang is a FAILURE**, never a skip: a machine
//! without `erl` must not be able to turn the differential evidence into a vacuous
//! pass. The gate is:
//!
//! * the oracle is **required** by default — [`required`] is `true` unless the
//!   environment explicitly opts out;
//! * `BEAM_ETS_ALLOW_NO_ORACLE=1` is the **opt-out** for an environment that genuinely
//!   has no Erlang; it restores the old loud-skip behaviour (`oracle::skip` prints
//!   `[oracle] … SKIPPED …` to stderr and the test passes);
//! * `BEAM_ETS_REQUIRE_ORACLE=1` still **forces** the requirement (it overrides the
//!   opt-out), so a runner can be explicit about demanding the oracle;
//! * a `$ERTS_ORACLE` (or `$ERLC`) that points at a non-executable file is a **hard
//!   error** in every mode, since a misconfiguration must never be mistaken for "no
//!   oracle".
//!
//! Further conventions:
//!
//! * any scratch file lives under `env::temp_dir()` (i.e. `$TMPDIR`) and is removed
//!   afterwards, whether or not the oracle succeeded — [`Scratch`] is the RAII form
//!   that makes that true even on a panic, and [`compile_module`] compiles an Erlang
//!   module into one with the oracle's `erlc` ([`resolve_erlc`]);
//! * the child is run with a wall-clock bound, so a wedged oracle fails the test
//!   instead of hanging the suite.
//!
//! This module is compiled into every integration-test binary that declares
//! `mod common;`; unused helpers are covered by the parent module's `#![allow(dead_code)]`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// The env var that explicitly opts OUT of the oracle requirement.
///
/// A "truthy" value (set, non-empty, not `0`) accepts a missing oracle: `oracle::skip`
/// then prints a note and the test passes — the behaviour every oracle-less
/// environment must ask for explicitly.
pub const ALLOW_NO_ORACLE: &str = "BEAM_ETS_ALLOW_NO_ORACLE";

/// The env var that explicitly **forces** the oracle to be required.
///
/// Redundant with the default (required), but it overrides [`ALLOW_NO_ORACLE`], so a
/// runner can demand the oracle unambiguously.
pub const REQUIRE_ORACLE: &str = "BEAM_ETS_REQUIRE_ORACLE";

/// Whether the oracle is **mandatory** — the default.
///
/// `true` unless `BEAM_ETS_ALLOW_NO_ORACLE` explicitly opts out; `BEAM_ETS_REQUIRE_ORACLE`
/// always wins. See the module docs for why skipping must be opt-in.
pub fn required() -> bool {
    env_flag(REQUIRE_ORACLE) || !allow_no_oracle()
}

/// Whether `$BEAM_ETS_ALLOW_NO_ORACLE` explicitly accepts a missing oracle.
pub fn allow_no_oracle() -> bool {
    env_flag(ALLOW_NO_ORACLE)
}

/// A "truthy" environment flag: set, non-empty and not `0`.
fn env_flag(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// Report a skipped oracle cross-check, or **fail** when the oracle is required
/// (the default — see [`required`]).
///
/// Every caller passes its own test name, so both the failure and the skip note name
/// the check that did not run, and the failure says how to opt out.
pub fn skip(test: &str, why: &str) {
    if required() {
        panic!(
            "{test}: FAILED — {why}.\n\
             The oracle cross-checks are REQUIRED by default so that a machine without Erlang \
             cannot turn the differential evidence into a vacuous pass. Install Erlang, or point \
             $ERTS_ORACLE at an existing one, or set {ALLOW_NO_ORACLE}=1 to opt out explicitly."
        );
    }
    eprintln!("[oracle] {test}: SKIPPED — {why}. ({ALLOW_NO_ORACLE} is set.)");
}

/// Locate the oracle's `erl` from `$ERTS_ORACLE`, then `$PATH`.
///
/// A `$ERTS_ORACLE` that names a non-executable file is a **hard error** in every
/// mode: a misconfiguration must never be mistaken for "no oracle on this machine".
pub fn resolve_erl() -> Option<PathBuf> {
    match env::var_os("ERTS_ORACLE") {
        Some(value) => {
            let candidate = PathBuf::from(&value);
            assert!(
                is_executable(&candidate),
                "$ERTS_ORACLE is set to '{}', which is not an executable file",
                candidate.display()
            );
            Some(candidate)
        }
        None => find_in_path("erl"),
    }
}

/// Locate the oracle's `escript` from `$ERTS_ORACLE`, then `$PATH`.
pub fn resolve_escript() -> Option<PathBuf> {
    resolve_sibling_or_path(resolve_erl()?, "escript")
}

/// Locate the oracle's `erlc`: `$ERLC` when set, else the sibling of the resolved
/// `erl`, else the first `erlc` on `$PATH`.
///
/// `$ERLC` is the override the project's differential/benchmark tools use; like
/// `$ERTS_ORACLE`, a value that names a non-executable file is a **hard error** in
/// every mode. Without it, `erlc` normally sits beside `erl` (the sibling-first order
/// keeps the compiler with the runtime it will be paired against); the `$PATH`
/// fallback covers a wrapper that installs only `erl`.
pub fn resolve_erlc() -> Option<PathBuf> {
    if let Some(value) = env::var_os("ERLC") {
        let candidate = PathBuf::from(&value);
        assert!(
            is_executable(&candidate),
            "$ERLC is set to '{}', which is not an executable file",
            candidate.display()
        );
        return Some(candidate);
    }
    resolve_sibling_or_path(resolve_erl()?, "erlc")
}

/// `name` beside the resolved `erl`, else the first `name` on `$PATH`.
fn resolve_sibling_or_path(erl: PathBuf, name: &str) -> Option<PathBuf> {
    let sibling = erl.with_file_name(name);
    if is_executable(&sibling) {
        Some(sibling)
    } else {
        find_in_path(name)
    }
}

/// The first executable `name` on `$PATH`.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Whether `path` is an executable file.
pub fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// A scratch directory under `env::temp_dir()` (i.e. `$TMPDIR`), removed on drop.
///
/// The name is `beam_ets_<prefix>_<pid>_<n>` — the pid plus a process-global counter
/// keep binaries *and* concurrent callers within one binary from sharing a
/// directory. Any stale copy of the same name is removed first.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// Create the (fresh) scratch directory.
    pub fn new(prefix: &str) -> Scratch {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("beam_ets_{prefix}_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the oracle scratch directory is creatable");
        Scratch { dir }
    }

    /// The scratch directory path (hand it to `-pa`/`-o`/`-o DIR` as-is).
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Write `contents` to `name` inside the scratch directory, returning its path.
    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, contents)
            .unwrap_or_else(|error| panic!("cannot write the oracle file {}: {error}", path.display()));
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the
        // test, yet a cleanup failure is reported rather than swallowed.
        if let Err(error) = fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the oracle scratch dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compile the Erlang module `module` (source `source`) into `scratch` with `erlc`,
/// returning the compiled `<module>.beam` path.
///
/// A non-zero exit, a missing `.beam` or a child that does not exit within `timeout`
/// is a **hard failure** (the child's streams are in the panic message) — a broken
/// toolchain must never be mistaken for a skip. The [module docs](self) explain why.
pub fn compile_module(
    erlc: &Path,
    scratch: &Scratch,
    module: &str,
    source: &str,
    timeout: Duration,
) -> PathBuf {
    let name = format!("{module}.erl");
    let source_path = scratch.write(&name, source);

    let mut command = Command::new(erlc);
    command
        .arg("-o")
        .arg(scratch.path())
        .arg(&source_path)
        .current_dir(scratch.path());
    let output = run_bounded(&mut command, timeout).unwrap_or_else(|error| {
        panic!("erlc ({}) could not compile {name}: {error}", erlc.display())
    });

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "erlc ({}) failed on {name} with {}:\n{stderr}",
        erlc.display(),
        output.status
    );
    let beam = scratch.path().join(format!("{module}.beam"));
    assert!(
        beam.is_file(),
        "erlc ({}) produced no {}:\n{stderr}",
        erlc.display(),
        beam.display()
    );
    beam
}

/// Run `program` (an `escript` source, inline) with `args` and return its output.
///
/// `scratch` names the scratch directory, which is created under `env::temp_dir()`
/// (i.e. `$TMPDIR`) and removed afterwards, whether or not the oracle succeeded. A
/// non-zero exit is a hard failure (the child's streams are in the panic message); a
/// child that does not exit within `timeout` is killed and is also a hard failure.
pub fn run_escript(escript: &Path, program: &str, args: &[String], scratch: &str, timeout: Duration) -> Output {
    let dir = Scratch::new(scratch);
    let script = dir.write("oracle.escript", program);

    let mut command = Command::new(escript);
    command.arg(&script).args(args).current_dir(dir.path());
    let output = run_bounded(&mut command, timeout).unwrap_or_else(|error| {
        panic!("escript ({}) could not run the oracle: {error}", escript.display())
    });

    assert!(
        output.status.success(),
        "the oracle escript exited with {}:\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Parse an oracle `total -> <n>` line out of `stdout`.
pub fn parse_total(stdout: &[u8]) -> Option<i64> {
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("total -> ")?.trim().parse().ok())
}

/// Spawn `command` with both pipes captured and a wall-clock bound.
///
/// Both pipes are read only after the child has exited; the oracle programs used here
/// write a single line, far below the pipe capacity, so that cannot deadlock.
pub fn run_bounded(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|error| error.to_string()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {timeout:?}"));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
    }
}
