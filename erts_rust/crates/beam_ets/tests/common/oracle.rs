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
//! oracle** (`erl`/`escript` on `$PATH`, or whatever `$ERTS_ORACLE` points at).
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
//! * a `$ERTS_ORACLE` that points at a non-executable file is a **hard error** in every
//!   mode, since a misconfiguration must never be mistaken for "no oracle".
//!
//! Further conventions:
//!
//! * any scratch file lives under `env::temp_dir()` (i.e. `$TMPDIR`) and is removed
//!   afterwards, whether or not the oracle succeeded;
//! * the child is run with a wall-clock bound, so a wedged oracle fails the test
//!   instead of hanging the suite.
//!
//! This module is compiled into every integration-test binary that declares
//! `mod common;`; unused helpers are covered by the parent module's `#![allow(dead_code)]`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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

/// Locate the oracle's `escript` from `$ERTS_ORACLE`, then `$PATH`.
pub fn resolve_escript() -> Option<PathBuf> {
    let erl = match env::var_os("ERTS_ORACLE") {
        Some(value) => {
            let candidate = PathBuf::from(&value);
            assert!(
                is_executable(&candidate),
                "$ERTS_ORACLE is set to '{}', which is not an executable file",
                candidate.display()
            );
            candidate
        }
        None => find_in_path("erl")?,
    };
    let sibling = erl.with_file_name("escript");
    if is_executable(&sibling) {
        Some(sibling)
    } else {
        find_in_path("escript")
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

/// Run `program` (an `escript` source, inline) with `args` and return its output.
///
/// `scratch` names the scratch directory, which is created under `env::temp_dir()`
/// (i.e. `$TMPDIR`) as `beam_ets_<scratch>_<pid>` — the pid keeps binaries that run
/// concurrently from sharing it — and removed afterwards, whether or not the oracle
/// succeeded. A non-zero exit is a hard failure (the child's streams are in the
/// panic message); a child that does not exit within `timeout` is killed and is also
/// a hard failure.
pub fn run_escript(escript: &Path, program: &str, args: &[String], scratch: &str, timeout: Duration) -> Output {
    let dir = env::temp_dir().join(format!("beam_ets_{scratch}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the oracle scratch directory is creatable");
    let script = dir.join("oracle.escript");
    fs::write(&script, program).expect("the oracle program is writable");

    let mut command = Command::new(escript);
    command.arg(&script).args(args).current_dir(&dir);
    let output = run_bounded(&mut command, timeout).unwrap_or_else(|error| {
        panic!("escript ({}) could not run the oracle: {error}", escript.display())
    });
    let _ = fs::remove_dir_all(&dir);

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
