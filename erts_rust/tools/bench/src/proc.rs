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

//! Subprocess helpers: PATH resolution, timed runs with captured output, and a
//! non-invasive peak-memory (RSS high-water) poller.
//!
//! Everything here is pure `std` — no `which`, no `wait-timeout`, no `libc` — so it
//! stays inside the crate's "no new dependencies / `#![forbid(unsafe_code)]`" budget.
//!
//! Three subtleties are handled explicitly:
//!
//! * **No deadlock:** a child writing more than a pipe buffer to *either* stdout or
//!   stderr would block forever if we `wait()`ed before draining. We therefore spawn a
//!   reader thread per stream, collect both `String`s, and only then join the child.
//! * **Peak memory:** Linux exposes `/proc/<pid>/status`'s `VmHWM` (peak resident set
//!   size). We poll it from a background thread *while the child runs* (once the
//!   process is reaped the file is gone and the peak is unreadable). If the field can
//!   never be read we report `None` — never a guess.
//! * **Transient `ETXTBSY`:** both spawn sites may execute a *freshly written*
//!   `#!/bin/sh` script (a wrapper compiler, a probe helper). When one thread `fork()`s
//!   while another still holds a descriptor open *for writing* that script, the kernel
//!   transiently refuses the `exec` with `ETXTBSY`. Both sites therefore spawn through
//!   [`spawn_with_retry`], which bounds a retry of that one error kind (see below).

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How many times a child spawn is attempted before a transient `ETXTBSY` is surfaced.
///
/// The race is short-lived (it lasts only until the thread that is writing the script
/// closes its descriptor), so a handful of attempts with a short backoff always suffices.
const SPAWN_ATTEMPTS: u32 = 5;

/// How long to wait between `ETXTBSY` spawn attempts.
const SPAWN_BACKOFF: Duration = Duration::from_millis(10);

/// The outcome of a completed subprocess run.
#[derive(Debug, Clone)]
pub struct Completed {
    /// The OS exit code, when the process terminated normally.
    pub exit_code: Option<i32>,
    /// Whether the process exited with status 0.
    pub success: bool,
    /// Captured stdout (UTF-8 lossy).
    pub stdout: String,
    /// Captured stderr (UTF-8 lossy).
    pub stderr: String,
    /// Wall-clock duration of the child, in milliseconds.
    pub wall_ms: f64,
    /// Peak resident set size in bytes, or `None` when `/proc` was unreadable.
    pub peak_memory_bytes: Option<u64>,
}

impl Completed {
    /// `stdout` with trailing whitespace trimmed.
    pub fn stdout_trim(&self) -> &str {
        self.stdout.trim()
    }
}

/// Searches `$PATH` for an executable named `prog`.
///
/// Returns the first matching *regular file*. There is no `which` crate and no
/// absolute-path assumption beyond the standard `PATH` split (no `unsafe`).
pub fn which(prog: &str) -> Option<PathBuf> {
    // An explicit path is honoured directly.
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

/// Runs a fully-configured [`Command`] to completion, capturing output, timing it and
/// sampling peak RSS.
pub fn run(cmd: &mut Command) -> std::io::Result<Completed> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = spawn_with_retry(cmd)?;
    let pid = child.id();

    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(0));
    let poller = spawn_peak_poller(pid, Arc::clone(&stop), Arc::clone(&peak));

    let out_handle = child.stdout.take().map(drain);
    let err_handle = child.stderr.take().map(drain);

    let status = child.wait();
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;

    stop.store(true, Ordering::Relaxed);
    let _ = poller.join();

    let stdout = out_handle.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    let stderr = err_handle.map(|h| h.join().unwrap_or_default()).unwrap_or_default();

    let status = status?;
    let peak_bytes = peak.load(Ordering::Relaxed);
    Ok(Completed {
        exit_code: status.code(),
        success: status.success(),
        stdout,
        stderr,
        wall_ms,
        peak_memory_bytes: if peak_bytes == 0 { None } else { Some(peak_bytes) },
    })
}

/// Spawns a child without waiting for it (used by the long-lived HTTP server driver).
pub fn spawn(cmd: &mut Command) -> std::io::Result<Child> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn_with_retry(cmd)
}

/// Spawn `cmd`, retrying **only** the transient `ETXTBSY` race.
///
/// Both spawn sites in this crate may legitimately execute a *freshly written*
/// `#!/bin/sh` script (a wrapper compiler, a self-check helper). When one thread
/// `fork()`s while another thread still holds an open descriptor **for writing** that
/// script, the child transiently inherits the descriptor; if the other thread then
/// `exec`s the script the kernel refuses with `ETXTBSY` ("Text file busy", errno 26).
/// This is a pure scheduling artifact — the file is fully written and executable — so
/// retrying after a short pause yields exactly the result the first attempt would have
/// produced had the other thread closed its descriptor first. It is therefore *not*
/// error swallowing: every non-`ETXTBSY` error, and the `ETXTBSY` that survives all
/// [`SPAWN_ATTEMPTS`] attempts, is returned unchanged for the caller to surface loudly.
fn spawn_with_retry(cmd: &mut Command) -> std::io::Result<Child> {
    let mut attempt = 1;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(err) if attempt < SPAWN_ATTEMPTS && is_transient_text_file_busy(&err) => {
                thread::sleep(SPAWN_BACKOFF);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Whether `err` is the transient `ETXTBSY` ("Text file busy") fork/exec race described on
/// [`spawn_with_retry`].
fn is_transient_text_file_busy(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::ExecutableFileBusy {
        return true;
    }
    // Belt and suspenders: accept the raw errno too, in case a platform/libc mapping does
    // not surface `ETXTBSY` as `ErrorKind::ExecutableFileBusy`.
    #[cfg(unix)]
    {
        const ETXTBSY: i32 = 26;
        if err.raw_os_error() == Some(ETXTBSY) {
            return true;
        }
    }
    false
}

/// Reads the child's `VmHWM` in a loop until told to stop, keeping the max observed.
fn spawn_peak_poller(pid: u32, stop: Arc<AtomicBool>, peak: Arc<AtomicU64>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Some(bytes) = read_vmhwm(pid) {
                peak.fetch_max(bytes, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(2));
        }
    })
}

/// Reads `/proc/<pid>/status`'s `VmHWM` (peak RSS) in bytes, if available.
///
/// Returns `None` on any non-Linux host or when the process is already gone.
pub fn read_vmhwm(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Drains a child stream into a `String` on its own thread (deadlock-free capture).
fn drain<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself when dropped (unique per thread, so the
    /// concurrent `cargo test` threads never collide).
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "bench-proc-{}-{}-{:?}",
                std::process::id(),
                tag,
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch { dir }
        }

        /// Writes an executable `#!/bin/sh` script `body` under this scratch dir.
        fn script(&self, name: &str, body: &str) -> PathBuf {
            let path = self.dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path).expect("stat").permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&path, perms).expect("chmod");
            }
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sh() -> Option<PathBuf> {
        which("sh")
    }
    #[test]
    fn which_finds_a_known_program_and_misses_a_bogus_one() {
        // `sh` is present on every POSIX test host.
        if let Some(p) = sh() {
            assert!(p.is_file());
        }
        assert!(which("definitely-not-a-real-program-xyzzy").is_none());
    }

    #[test]
    fn which_honours_explicit_paths() {
        let me = std::env::current_exe().unwrap();
        assert_eq!(which(&me.to_string_lossy()), Some(me.clone()));
        assert!(which("/nonexistent/path/to/thing").is_none());
    }

    #[test]
    fn run_captures_stdout_stderr_and_exit_code() {
        let Some(sh) = sh() else { return };
        // Write a lot to both streams to exercise the deadlock-free capture path.
        let mut cmd = Command::new(&sh);
        cmd.args(["-c", "yes out | head -n 5000; yes err | head -n 5000 >&2; exit 3"]);
        let out = run(&mut cmd).unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.success);
        assert!(out.stdout.contains("out"));
        assert!(out.stderr.contains("err"));
        assert!(out.wall_ms >= 0.0);
    }

    #[test]
    fn run_times_a_sleep() {
        let Some(sh) = sh() else { return };
        let mut cmd = Command::new(&sh);
        cmd.args(["-c", "sleep 0.05"]);
        let out = run(&mut cmd).unwrap();
        assert!(out.success);
        assert!(out.wall_ms >= 20.0, "expected >=20ms, got {}", out.wall_ms);
    }

    #[test]
    fn peak_poller_reports_none_or_a_positive_number() {
        let Some(sh) = sh() else { return };
        let mut cmd = Command::new(&sh);
        cmd.args(["-c", "sleep 0.05"]);
        let out = run(&mut cmd).unwrap();
        if let Some(bytes) = out.peak_memory_bytes {
            assert!(bytes > 0);
        }
    }

    #[test]
    fn read_vmhwm_of_missing_pid_is_none() {
        // A definitely-nonexistent pid: no /proc entry, must not panic.
        assert_eq!(read_vmhwm(u32::MAX), None);
    }

    #[test]
    fn spawn_with_retry_passes_a_successful_run_through_unchanged() {
        let dir = Scratch::new("retry-ok");
        let script = dir.script("probe", "echo out");
        let mut cmd = Command::new(&script);
        let out = run(&mut cmd).expect("spawn");
        assert!(out.success);
        assert_eq!(out.stdout_trim(), "out");
    }

    #[test]
    fn spawn_with_retry_surfaces_a_non_transient_error_loudly() {
        // A missing executable is not an ETXTBSY race: it must come straight back, so the
        // bounded retry can never mask (or delay) a genuine spawn failure.
        let mut cmd = Command::new("/nonexistent/bench-proc-retry-test");
        let err = spawn_with_retry(&mut cmd).expect_err("must fail to spawn");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err:?}");
    }

    #[test]
    fn is_transient_text_file_busy_matches_only_etxtbsy() {
        assert!(is_transient_text_file_busy(&std::io::Error::from(
            std::io::ErrorKind::ExecutableFileBusy
        )));
        assert!(is_transient_text_file_busy(
            &std::io::Error::from_raw_os_error(26)
        ));
        assert!(!is_transient_text_file_busy(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(!is_transient_text_file_busy(
            &std::io::Error::from_raw_os_error(13)
        ));
    }

    /// Deterministically exercises the `ETXTBSY` path the retry exists for.
    ///
    /// Linux refuses to `exec` a file that **any** process (including this one) holds open
    /// for writing, with `ETXTBSY` — exactly the transient error a concurrent `fork()` can
    /// cause for a freshly written script. Holding the descriptor open therefore makes
    /// *every* attempt fail, which pins down two behaviors a timing-dependent test could
    /// not: the retry is **bounded** (it gives up and returns the error instead of spinning
    /// or swallowing it), and once the writer is gone the very same command runs.
    #[cfg(unix)]
    #[test]
    fn spawn_with_retry_is_bounded_on_etxtbsy_and_surfaces_it() {
        let dir = Scratch::new("retry-etxtbsy");
        let script = dir.script("held-open", "echo out");
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&script)
            .expect("open the script for writing");

        let mut cmd = Command::new(&script);
        let err = spawn_with_retry(&mut cmd).expect_err("a write-open file cannot be exec'd");
        assert!(
            is_transient_text_file_busy(&err),
            "expected ETXTBSY, got {err:?}"
        );

        drop(held);
        let mut cmd = Command::new(&script);
        let out = run(&mut cmd).expect("spawn once the writer closed");
        assert_eq!(out.stdout_trim(), "out");
    }
}
