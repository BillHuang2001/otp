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

//! The **live-`erl` oracle harness** the C-driver suites share.
//!
//! A `.so` compiled against the unmodified `erl_driver.h` is the one artifact both runtimes can be
//! handed, so the way to pin a driver C-ABI behaviour against the real system is to load that same
//! object under the system Erlang (`erl_ddll:load_driver/2` + `open_port({spawn_driver, Name}, …)`
//! + whatever BIF the row is about) and compare its observable answers with this crate's. That is
//! exactly what [`run_escript`] does: it writes the caller's escript, runs it with the object's
//! directory and base name as the only argument, and returns its stdout.
//!
//! Each suite keeps its **own** escript text (the rows belong to the suite), so this module carries
//! the spawn/timeout/skip policy once instead of per suite — `tests/abi_failure.rs`'s
//! `driver_output*` rows and `tests/abi_driver.rs`'s `port_control/3` shape rows differ only in the
//! Erlang body they hand in.
//!
//! # Skip-vs-fail policy
//!
//! * no `escript`/`erl` on `PATH`, or no answer within the caller's timeout ⇒ a **loud** [`skip`]
//!   banner and `None` (the caller must then return, not silently pass);
//! * a non-zero exit from the escript, or unparsable output ⇒ an **assertion failure** — a broken
//!   oracle session is a divergence in the harness, never a skip.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::{scratch_dir, skip};

/// Is `tool` an executable on `PATH`?
pub fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .any(|candidate| candidate.is_file())
    })
}

/// Writes `body` into a fresh scratch directory as `<name>.escript` and answers its path.
///
/// The directory is fresh per call ([`scratch_dir`]), so a suite that runs two sessions never has
/// one script overwrite the other.
pub fn write_escript(name: &str, body: &str) -> PathBuf {
    let dir = scratch_dir(name);
    std::fs::create_dir_all(&dir).expect("a scratch dir is creatable");
    let script = dir.join(format!("{name}.escript"));
    std::fs::write(&script, body).expect("the oracle escript is writable");
    script
}

/// Runs `escript <script> <so>` (with `cwd` as its working directory) and answers its stdout.
///
/// The escript receives the compiled fixture's path as its only argument, so it can derive the
/// directory and the base name `erl_ddll:load_driver/2` needs. `cwd` is the suite's business: a
/// fixture that wrote outside the paths it was given would leave its file there, which a caller
/// asserts against by handing in an empty sandbox.
///
/// The session runs on its own thread so a wedged runtime cannot hang the test: `timeout` bounds the
/// wait, and a timeout is a loud **skip** (the oracle is unavailable, not wrong). A non-zero exit is
/// a failed assertion — that is the oracle speaking, and disagreeing with it is a divergence.
pub fn run_escript(
    name: &str,
    body: &str,
    so: &Path,
    cwd: &Path,
    timeout: Duration,
) -> Option<String> {
    let script = write_escript(name, body);
    let so = so.to_path_buf();
    let cwd = cwd.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out = Command::new("escript")
            .arg(&script)
            .arg(&so)
            .current_dir(&cwd)
            .output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(timeout) {
        Err(_) => {
            skip(&format!(
                "oracle check skipped: the `escript` session did not answer within {timeout:?}"
            ));
            None
        }
        Ok(Err(err)) => {
            skip(&format!(
                "oracle check skipped: could not spawn `escript` ({err}); is escript on PATH?"
            ));
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle escript failed (exit {:?}) — a non-zero exit is a divergence, never a \
                 skip:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}
