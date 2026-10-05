//! Subprocess helpers: PATH resolution, timed runs with captured output, and a
//! non-invasive peak-memory (RSS high-water) poller.
//!
//! Everything here is pure `std` — no `which`, no `wait-timeout`, no `libc` — so it
//! stays inside the crate's "no new dependencies / `#![forbid(unsafe_code)]`" budget.
//!
//! Two subtleties are handled explicitly:
//!
//! * **No deadlock:** a child writing more than a pipe buffer to *either* stdout or
//!   stderr would block forever if we `wait()`ed before draining. We therefore spawn a
//!   reader thread per stream, collect both `String`s, and only then join the child.
//! * **Peak memory:** Linux exposes `/proc/<pid>/status`'s `VmHWM` (peak resident set
//!   size). We poll it from a background thread *while the child runs* (once the
//!   process is reaped the file is gone and the peak is unreadable). If the field can
//!   never be read we report `None` — never a guess.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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
    let mut child = cmd.spawn()?;
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
    cmd.spawn()
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
}
