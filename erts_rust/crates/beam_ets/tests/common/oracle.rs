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
//! Every test that needs the real `ets` follows the same conventions:
//!
//! * a genuinely absent Erlang is a **loud skip** (`eprintln!` of what was skipped and
//!   why), turned into a failure by `BEAM_ETS_REQUIRE_ORACLE=1` — for CI, where a
//!   silently skipped oracle must not look like evidence;
//! * a `$ERTS_ORACLE` that points at a non-executable file is a **hard error**, since a
//!   misconfiguration must never be mistaken for "no oracle";
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

/// Whether `$BEAM_ETS_REQUIRE_ORACLE` asks for the oracle to be mandatory.
///
/// An unset, empty or `0` value means "skipping is acceptable".
pub fn required() -> bool {
    match env::var("BEAM_ETS_REQUIRE_ORACLE") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// Report a skipped oracle cross-check, or panic when the oracle was required.
///
/// Every caller passes its own test name, so the printed note names the check that
/// did not run.
pub fn skip(test: &str, why: &str) {
    let note = format!("{test}: SKIPPED — {why}.");
    if required() {
        panic!("{note} $BEAM_ETS_REQUIRE_ORACLE is set, so this is a failure.");
    }
    eprintln!("[oracle] {note}");
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
