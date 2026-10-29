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
//! Subprocess helpers: `PATH` resolution, timed runs with fully captured output, and the
//! host-load probes recorded around every sweep.
//!
//! Pure `std` — no `libc`, no `wait-timeout`. Two subtleties are handled explicitly:
//!
//! * **No deadlock.** A child that writes more than a pipe buffer to *either* stdout or
//!   stderr would block forever if we waited before draining, so each stream is drained on
//!   its own thread and the strings are collected after the child is reaped.
//! * **No silent hangs.** Every run is bounded by a caller-supplied timeout; a run that
//!   exceeds it is killed and reported as `timed_out`, never as an empty success. (The kill
//!   targets the direct child; a child that *daemonizes* and leaves a grandchild holding the
//!   pipe open would keep the drain threads alive until that grandchild exits. The two
//!   runtimes driven here — `erl` and `beam_rt` — never daemonize, and a killed run is still
//!   reported as `timed_out`.)
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
/// How long to wait between `try_wait` polls.
const POLL: Duration = Duration::from_millis(10);
/// The outcome of a completed (or killed) subprocess run.
#[derive(Debug, Clone, Default)]
pub struct Completed {
    /// The OS exit code, when the process terminated normally.
    pub exit_code: Option<i32>,
    /// Whether the process exited with status 0 (false when it was killed by a signal).
    pub success: bool,
    /// Whether the harness killed the process after the timeout expired.
    pub timed_out: bool,
    /// Captured stdout (UTF-8 lossy).
    pub stdout: String,
    /// Captured stderr (UTF-8 lossy).
    pub stderr: String,
    /// Wall-clock duration in milliseconds.
    pub wall_ms: f64,
}
impl Completed {
    /// `stdout` with trailing whitespace trimmed.
    pub fn stdout_trim(&self) -> &str {
        self.stdout.trim()
    }
    /// The last `n` non-empty lines of a stream, joined — used for the "raw tail" that makes
    /// a failed row diagnosable in the report.
    pub fn tail(&self, n: usize) -> String {
        let text = if self.stdout.trim().is_empty() { &self.stderr } else { &self.stdout };
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }
}
/// Searches `$PATH` for an executable named `prog` (an explicit path with a `/` is
/// honoured directly). Returns the first matching regular file.
pub fn which(prog: &str) -> Option<PathBuf> {
    if prog.contains('/') {
        let p = PathBuf::from(prog);
        return if p.is_file() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
/// Glob-free expansion of `/nix/store/*<needle>*/bin/<prog>`: the sandbox often has
/// `rebar3` only in the Nix store, with the hash unknown ahead of time.
pub fn find_in_nix_store(needle: &str, prog: &str) -> Option<PathBuf> {
    let store = PathBuf::from("/nix/store");
    let rd = std::fs::read_dir(&store).ok()?;
    let mut best: Option<PathBuf> = None;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.contains(needle) {
            continue;
        }
        let candidate = entry.path().join("bin").join(prog);
        if candidate.is_file() {
            // Prefer the lexicographically greatest name (a later version sorts higher among
            // the usual `<hash>-<name>-<version>` entries); determinism beats newest-unknown.
            match &best {
                Some(prev) if prev.to_string_lossy() >= candidate.to_string_lossy() => {}
                _ => best = Some(candidate),
            }
        }
    }
    best
}
/// Runs a fully-configured [`Command`] to completion, capturing both streams.
pub fn run(cmd: &mut Command) -> std::io::Result<Completed> {
    run_with_timeout(cmd, None)
}
/// Runs a fully-configured [`Command`], killing it once `timeout` elapses.
///
/// A timeout is **not** an error: the returned [`Completed`] has `timed_out = true` and the
/// output captured so far, which the callers classify as a `timeout` row.
pub fn run_with_timeout(cmd: &mut Command, timeout: Option<Duration>) -> std::io::Result<Completed> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let out_handle = child.stdout.take().map(drain);
    let err_handle = child.stderr.take().map(drain);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                return Err(err);
            }
        }
        if let Some(limit) = timeout {
            if start.elapsed() >= limit {
                timed_out = true;
                let _ = child.kill();
                break child.wait()?;
            }
        }
        thread::sleep(POLL);
    };
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    // The drain threads only run `read_to_end` plus a lossy UTF-8 conversion, neither of
    // which can panic, so a join failure is impossible by construction; `unwrap_or_default`
    // cannot mask a real failure here (same reasoning as `tools/bench/src/proc.rs`).
    let stdout = out_handle.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    let stderr = err_handle.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    Ok(Completed {
        exit_code: status.code(),
        success: status.success() && !timed_out,
        timed_out,
        stdout,
        stderr,
        wall_ms,
    })
}
fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}
/// The kernel load average as recorded around every sweep (`None` off Linux).
///
/// The host is shared, so every result file carries these; the report never quotes an
/// absolute wall time as if it were portable.
pub fn loadavg() -> Option<String> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    Some(text.trim().to_string())
}
/// The number of logical CPUs the host advertises, when known.
pub fn nproc() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let n = text.lines().filter(|l| l.starts_with("processor")).count();
    if n == 0 {
        None
    } else {
        Some(n as u64)
    }
}
/// Seconds since the Unix epoch (the harness stamps the report with this; formatting a
/// human date would need `chrono`, which the crate deliberately does not depend on).
pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
/// A shell-quoted rendering of a [`Command`], for the report's "exact command" column.
pub fn cmdline(cmd: &Command) -> String {
    let mut parts = vec![quote(&cmd.get_program().to_string_lossy())];
    for arg in cmd.get_args() {
        parts.push(quote(&arg.to_string_lossy()));
    }
    parts.join(" ")
}
fn quote(s: &str) -> String {
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"' || c == '$' || c == '`' || c == '\\' || c == ';' || c == '&' || c == '|' || c == '*' || c == '?' || c == '[' || c == ']' || c == '(' || c == ')' || c == '<' || c == '>') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timeout_kills_and_flags() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 5");
        let started = Instant::now();
        let done = run_with_timeout(&mut cmd, Some(Duration::from_millis(200))).expect("spawn sh");
        assert!(done.timed_out, "expected a timeout");
        assert!(!done.success);
        assert!(started.elapsed() < Duration::from_secs(4), "kill must be prompt");
    }
    #[test]
    fn captures_both_streams() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo out; echo err >&2; exit 3");
        let done = run(&mut cmd).expect("spawn sh");
        assert_eq!(done.exit_code, Some(3));
        assert!(!done.success);
        assert_eq!(done.stdout.trim(), "out");
        assert_eq!(done.stderr.trim(), "err");
        assert!(done.tail(1).contains("out"));
    }
    #[test]
    fn quotes_awkward_arguments() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
