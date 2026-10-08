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

//! The **crash-dump** end-to-end: `erlang:halt/0,1,2` and the `ERL_CRASH_DUMP`
//! environment, driven through the built binary.
//!
//! `src/crash_dump.rs` proves the emitter in isolation (its unit tests render a synthetic
//! [`DumpInput`](beam_rt::crash_dump::DumpInput)); this suite proves the **runtime
//! behaviour** around it: which `halt/1,2` shapes terminate with a status, an `abort` or a
//! dump, and that `ERL_CRASH_DUMP` / `ERL_CRASH_DUMP_SECONDS` name (or suppress) the file
//! the emulator writes before it exits — the oracle's own rules
//! (`erts/emulator/beam/{bif.c,erl_init.c,erl_crash_dump.c}`).
//!
//! Every case runs in a **private scratch directory** (so a dump is found where the
//! child's working directory says it should be, and never in the repository) and every
//! child is **bounded and killed** by [`run_bounded`], so a runtime that hung would fail
//! the test instead of hanging the suite.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The built runtime every case drives.
const BEAM_RT: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The emulator's default dump file name (`erl_crash_dump.c:504`).
const DEFAULT_DUMP: &str = "erl_crash.dump";

/// The crash-dump header every dump starts with (`=erl_crash_dump:0.5`).
const DUMP_HEADER: &str = "=erl_crash_dump:0.5";

/// How long one child may run before it is killed.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

/// Distinguishes scratch directories created within one process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A private scratch directory, removed when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Creates an empty, uniquely named directory under the system temp directory.
    fn new() -> Scratch {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is past the epoch")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_crash_dump_{}_{stamp}_{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a scratch directory");
        Scratch { path }
    }

    /// The directory itself.
    fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory.
    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// The names of the directory's entries, sorted (so the assertion messages are stable).
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.path)
            .expect("a readable scratch directory")
            .map(|entry| {
                entry
                    .expect("a readable directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// The text of the dump file `name` in this directory.
    fn dump(&self, name: &str) -> String {
        std::fs::read_to_string(self.join(name))
            .unwrap_or_else(|error| panic!("{name} in {}: {error}", self.path.display()))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A finished child: its status and its stderr.
struct Run {
    status: ExitStatus,
    stderr: String,
}

impl Run {
    /// The exit code, when the child exited normally.
    fn code(&self) -> Option<i32> {
        self.status.code()
    }    /// The signal that terminated the child, when one did.
    #[cfg(unix)]
    fn signal(&self) -> Option<i32> {
        use std::os::unix::process::ExitStatusExt;
        self.status.signal()
    }

    /// The signal that terminated the child: platforms without signals never report one.
    #[cfg(not(unix))]
    fn signal(&self) -> Option<i32> {
        None
    }
}

/// Runs the built runtime in `dir`, with `env` overrides, bounded by [`CHILD_TIMEOUT`].
///
/// The `ERL_CRASH_DUMP` variables are **removed** from the inherited environment first, so
/// an ambient value can never decide what these cases observe; `env` then sets exactly the
/// ones a case asks for.
fn beam_rt(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut command = Command::new(BEAM_RT);
    command
        .current_dir(dir)
        .args(args)
        .env_remove("ERL_CRASH_DUMP")
        .env_remove("ERL_CRASH_DUMP_SECONDS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    run_bounded(command)
}

/// Spawns `command` and waits at most [`CHILD_TIMEOUT`], killing (and reaping) it if it
/// outlives that.
fn run_bounded(mut command: Command) -> Run {
    let mut child = command.spawn().expect("spawning beam_rt");
    let deadline = Instant::now() + CHILD_TIMEOUT;
    let status = loop {
        match child.try_wait().expect("waiting for beam_rt") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let status = child.wait().expect("reaping the killed beam_rt");
                panic!("beam_rt outlived {CHILD_TIMEOUT:?} and was killed ({status})");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stderr);
    }
    Run { status, stderr }
}

/// The section tags of a dump, in order (`=proc:<pid>`, `=end`, …).
fn sections(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with('='))
        .collect()
}

#[test]
fn a_halt_slogan_writes_a_crash_dump_and_exits_one() {
    let scratch = Scratch::new();
    let run = beam_rt(scratch.path(), &[], &["-eval", "halt(\"probe\")."]);
    assert_eq!(run.code(), Some(1), "stderr: {}", run.stderr);

    // The oracle's announcement: the slogan, then the dump's name (`erl_crash_dump.c:509`).
    assert!(run.stderr.starts_with("probe"), "stderr: {:?}", run.stderr);
    assert!(
        run.stderr.contains("Crash dump is being written to:"),
        "stderr: {:?}",
        run.stderr
    );
    assert!(
        run.stderr.contains(DEFAULT_DUMP),
        "stderr: {:?}",
        run.stderr
    );

    // The dump parses back: the header, the slogan, at least one process section and the
    // terminating `=end`.
    let text = scratch.dump(DEFAULT_DUMP);
    assert_eq!(text.lines().next(), Some(DUMP_HEADER), "{text:.200}");
    assert!(text.contains("\nSlogan: probe\n"), "{text:.400}");
    let tags = sections(&text);
    assert!(
        tags.iter().any(|tag| tag.starts_with("=proc:")),
        "no =proc: section in {:?}",
        &tags[..tags.len().min(20)]
    );
    assert_eq!(tags.last(), Some(&"=end"), "the dump ends with =end");
}

#[test]
fn an_uncaught_raise_writes_a_crash_dump_and_exits_one() {
    let scratch = Scratch::new();
    let run = beam_rt(scratch.path(), &[], &["-eval", "1/0."]);
    assert_eq!(run.code(), Some(1), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("badarith"), "stderr: {}", run.stderr);
    let text = scratch.dump(DEFAULT_DUMP);
    assert_eq!(text.lines().next(), Some(DUMP_HEADER));
    assert_eq!(sections(&text).last(), Some(&"=end"));
}

#[test]
fn erl_crash_dump_names_the_file() {
    let scratch = Scratch::new();
    let named = scratch.join("named.dump");
    let name = named.to_string_lossy().into_owned();
    let run = beam_rt(
        scratch.path(),
        &[("ERL_CRASH_DUMP", &name)],
        &["-eval", "halt(\"named\")."],
    );
    assert_eq!(run.code(), Some(1), "stderr: {}", run.stderr);
    // The named file — and only that file — holds the dump.
    let text = scratch.dump("named.dump");
    assert_eq!(text.lines().next(), Some(DUMP_HEADER));
    assert!(text.contains("\nSlogan: named\n"));
    assert_eq!(scratch.entries(), vec!["named.dump".to_string()]);
}

#[test]
fn erl_crash_dump_seconds_zero_writes_no_dump() {
    let scratch = Scratch::new();
    let run = beam_rt(
        scratch.path(),
        &[("ERL_CRASH_DUMP_SECONDS", "0")],
        &["-eval", "halt(\"suppressed\")."],
    );
    // The exit classification is unchanged; only the dump is suppressed.
    assert_eq!(run.code(), Some(1), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("suppressed"), "stderr: {}", run.stderr);
    assert_eq!(scratch.entries(), Vec::<String>::new());
}

#[test]
fn a_status_halt_exits_with_that_status_and_writes_no_dump() {
    let scratch = Scratch::new();
    let run = beam_rt(scratch.path(), &[], &["-eval", "halt(17)."]);
    assert_eq!(run.code(), Some(17), "stderr: {}", run.stderr);
    assert!(run.stderr.is_empty(), "stderr: {}", run.stderr);
    assert_eq!(scratch.entries(), Vec::<String>::new());
}

#[test]
fn a_status_beyond_the_byte_is_truncated_by_the_os() {
    // `halt(300)` asks for 300; the OS keeps the low byte (44), exactly as the oracle's
    // own `halt(300)` does.
    let scratch = Scratch::new();
    let run = beam_rt(scratch.path(), &[], &["-eval", "halt(300)."]);
    assert_eq!(run.code(), Some(44), "stderr: {}", run.stderr);
    assert_eq!(scratch.entries(), Vec::<String>::new());
}

#[test]
fn halt_abort_terminates_the_runtime_with_sigabrt() {
    let scratch = Scratch::new();
    let run = beam_rt(scratch.path(), &[], &["-eval", "halt(abort)."]);
    // `erts_exit(ERTS_ABORT_EXIT, "")` → `abort()`: a shell sees 134 (128 + SIGABRT).
    assert!(
        run.code() == Some(134) || run.signal() == Some(6),
        "code {:?}, signal {:?}: {}",
        run.code(),
        run.signal(),
        run.stderr
    );
    // The abort path writes the dump *before* it aborts.
    let text = scratch.dump(DEFAULT_DUMP);
    assert_eq!(text.lines().next(), Some(DUMP_HEADER));
}

#[test]
fn a_zero_flush_timeout_halts_with_status_255() {
    let scratch = Scratch::new();
    let run = beam_rt(
        scratch.path(),
        &[],
        &["-eval", "halt(1, [{flush_timeout, 0}])."],
    );
    assert_eq!(run.code(), Some(255), "stderr: {}", run.stderr);
    assert_eq!(scratch.entries(), Vec::<String>::new());
}
