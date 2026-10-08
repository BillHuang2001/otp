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

//! `cowboy` — the HTTP I/O-bound workload (the Cowboy `hello_world` example).
//!
//! The Cowboy checkout is a **read-only** foreign repository, so the workload never
//! builds it in place: it stages a copy into a private
//! [`ScratchDir`](crate::scratch::ScratchDir) under `$TMPDIR`, builds the
//! `examples/hello_world` relx release *there*, starts it on port 8080, and drives it
//! with the crate's own dependency-free HTTP/1.1 load generator ([`crate::http_load`]).
//! No external `wrk`/`h2load`/`bombardier` is needed — the in-process client is the
//! generator. The workload records throughput (req/s), the p50/p99 latency
//! distribution, the server process's peak RSS and the build time; the scratch tree
//! (and the untouched Cowboy source) is left clean afterwards.
//!
//! Repo: `$BENCH_COWBOY_REPO`, else `/home/bill/Source/cowboy`.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::http_load::{self, LoadConfig};
use crate::metrics::{Category, Metric, Probe, Runtimes};
use crate::proc;
use crate::report::{Profile, WorkloadResult};
use crate::scratch::{self, ScratchDir};
use crate::workload::{RunContext, Workload};
use crate::workloads::resolve_repo;

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/cowboy";
/// The port the `hello_world` example listens on.
const PORT: u16 = 8080;
/// How long to wait for the server to start accepting connections.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Concurrency (connections) and duration of the `--quick` load run.
const QUICK_CONNECTIONS: usize = 8;
const QUICK_DURATION: Duration = Duration::from_secs(2);
/// Concurrency (connections) and duration of the `--full` load run.
const FULL_CONNECTIONS: usize = 16;
const FULL_DURATION: Duration = Duration::from_secs(5);
/// Upper bound on stored latency samples (keeps the report bounded).
const MAX_LATENCY_SAMPLES: usize = 20_000;
/// How much of the `make` output to embed in a failure/skip note.
const OUTPUT_TAIL_CHARS: usize = 400;
/// How often the peak-RSS poller samples the server process.
const PEAK_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// The documented relx release start script, relative to `examples/hello_world`.
const RELEASE_SCRIPT: &str = "_rel/hello_world_example/bin/hello_world_example";

/// The Cowboy HTTP workload.
pub struct Cowboy;

fn repo() -> PathBuf {
    resolve_repo("cowboy", DEFAULT_REPO)
}

impl Workload for Cowboy {
    fn id(&self) -> &'static str {
        "cowboy"
    }

    fn description(&self) -> &'static str {
        "Cowboy hello_world HTTP server driven by the in-process HTTP/1.1 load generator"
    }

    fn category(&self) -> Category {
        Category::Io
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.6"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::SysOnly
    }

    fn probe(&self) -> Probe {
        if !repo().is_dir() {
            return Probe::skipped("repo-missing");
        }
        if proc::which("make").is_none() {
            return Probe::skipped("make-absent");
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let r = repo();
        if !r.is_dir() {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "repo-missing",
            );
        }
        let Some(make) = proc::which("make") else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "make-absent",
            );
        };

        // Stage the read-only checkout into a scratch dir; the build runs only there.
        let scratch = match ScratchDir::new("cowboy") {
            Ok(s) => s,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("scratch-failed: {e}"),
                )
            }
        };
        let staged = scratch.path().join("cowboy");
        if let Err(e) = scratch::copy_tree(&r, &staged, &[".git", "_rel"]) {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("copy-failed: {e}"),
            );
        }
        let example = staged.join("examples/hello_world");
        if !example.is_dir() {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "example-dir-missing",
            );
        }

        // Build the relx release (this also git-fetches the Cowboy/cowlib/ranch deps).
        let mut build_cmd = Command::new(&make);
        build_cmd.current_dir(&example);
        let build = match proc::run(&mut build_cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("make-spawn-failed: {e}"),
                )
            }
        };
        if !build.success {
            let output = format!("{}\n{}", build.stdout, build.stderr);
            let tail = tail(&output, OUTPUT_TAIL_CHARS);
            if looks_like_deps_failure(&output) {
                let mut result = WorkloadResult::skipped(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    "deps-unavailable",
                );
                result
                    .metrics
                    .push(Metric::scalar("build_ms", "ms", build.wall_ms));
                result.notes.push(format!("make output tail: {tail}"));
                return result;
            }
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("build-failed: {tail}"),
            );
        }

        // Locate and start the release.
        let Some(script) = find_release_script(&example) else {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "release-missing-after-build",
            );
        };
        let mut server_cmd = Command::new(&script);
        server_cmd.current_dir(&example).arg("foreground");
        let mut server = match proc::spawn(&mut server_cmd) {
            Ok(c) => c,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("server-spawn-failed: {e}"),
                )
            }
        };
        let server_pid = server.id();
        // The server can log to stdout/stderr; drain those pipes so a full buffer can
        // never block the VM mid-run.
        drain_child(&mut server);

        match wait_for_server(&mut server, PORT, READY_TIMEOUT) {
            ServerWait::Ready => {}
            ServerWait::Exited => {
                stop_child(&mut server);
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    "server-exited-early",
                );
            }
            ServerWait::Timeout => {
                stop_child(&mut server);
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    "server-not-ready",
                );
            }
        }

        // Poll the server's peak RSS while it serves.
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let poller = {
            let stop = Arc::clone(&stop);
            let peak = Arc::clone(&peak);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(bytes) = proc::read_vmhwm(server_pid) {
                        peak.fetch_max(bytes, Ordering::Relaxed);
                    }
                    thread::sleep(PEAK_POLL_INTERVAL);
                }
            })
        };

        // Drive the server with the in-process load generator.
        let (connections, duration) = match ctx.profile {
            Profile::Quick => (QUICK_CONNECTIONS, QUICK_DURATION),
            Profile::Full => (FULL_CONNECTIONS, FULL_DURATION),
        };
        let mut cfg = LoadConfig::new(SocketAddr::from(([127, 0, 0, 1], PORT)), "/");
        cfg.connections = connections;
        cfg.duration = duration;
        cfg.max_latency_samples = MAX_LATENCY_SAMPLES;
        let load = http_load::drive(&cfg);

        // Stop the server and finish the peak poller.
        stop_child(&mut server);
        stop.store(true, Ordering::Relaxed);
        let _ = poller.join();
        let peak_bytes = match peak.load(Ordering::Relaxed) {
            0 => None,
            bytes => Some(bytes),
        };

        let outcome = match load {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("load-failed: {e}"),
                )
            }
        };

        if outcome.requests == 0 {
            let mut result = WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "load-no-responses",
            );
            result
                .metrics
                .push(Metric::scalar("build_ms", "ms", build.wall_ms));
            result
                .metrics
                .push(Metric::scalar("errors", "count", outcome.errors as f64));
            result.notes.push(format!(
                "0 responses in {:.0} ms of load ({} transport errors)",
                outcome.wall_ms, outcome.errors
            ));
            result.peak_memory_bytes = peak_bytes;
            return result;
        }

        let seconds = (outcome.wall_ms / 1000.0).max(f64::MIN_POSITIVE);
        let requests_per_sec = outcome.requests as f64 / seconds;

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result
            .metrics
            .push(Metric::scalar("build_ms", "ms", build.wall_ms));
        result
            .metrics
            .push(Metric::scalar("requests", "count", outcome.requests as f64));
        result
            .metrics
            .push(Metric::scalar("errors", "count", outcome.errors as f64));
        result.metrics.push(Metric::scalar(
            "requests_per_sec",
            "req/s",
            requests_per_sec,
        ));
        result
            .metrics
            .push(Metric::samples("latency_ms", "ms", &outcome.latencies_ms));
        result.peak_memory_bytes = peak_bytes;
        if outcome.latencies_ms.is_empty() {
            result
                .notes
                .push("no latency samples recorded".to_string());
        }
        result
    }
}

/// How the server behaved while we waited for its port to open.
enum ServerWait {
    /// The port accepts connections.
    Ready,
    /// The server process exited before the port opened.
    Exited,
    /// Neither happened within the timeout.
    Timeout,
}

/// Polls the server until its port accepts a connection, it exits, or `timeout` elapses.
fn wait_for_server(server: &mut Child, port: u16, timeout: Duration) -> ServerWait {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(Some(_status)) = server.try_wait() {
            return ServerWait::Exited;
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return ServerWait::Ready;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ServerWait::Timeout
}

/// Kills and reaps the server child (best-effort; a failure here is not a benchmark
/// error — the process is gone either way once we move on).
fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Drains the child's stdout/stderr on background threads so a full pipe buffer can
/// never block the server. The threads end when the child (and its pipes) close.
fn drain_child(child: &mut Child) {
    if let Some(mut out) = child.stdout.take() {
        thread::spawn(move || {
            let _ = std::io::copy(&mut out, &mut std::io::sink());
        });
    }
    if let Some(mut err) = child.stderr.take() {
        thread::spawn(move || {
            let _ = std::io::copy(&mut err, &mut std::io::sink());
        });
    }
}

/// Finds the relx release start script: the documented path first, else the first
/// `<name>/bin/<name>` under `_rel` (relx releases are named after `relx.config`).
fn find_release_script(example: &Path) -> Option<PathBuf> {
    let documented = example.join(RELEASE_SCRIPT);
    if documented.is_file() {
        return Some(documented);
    }
    for entry in std::fs::read_dir(example.join("_rel")).ok()?.flatten() {
        let candidate = entry.path().join("bin").join(entry.file_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The last `max_chars` characters of `text` (trimmed), for a bounded failure note.
fn tail(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }
    trimmed.chars().skip(count - max_chars).collect()
}

/// Whether `make`'s output looks like a dependency/network fetch failure (as opposed
/// to a genuine compile error), so the workload can be `skipped` rather than `failed`.
fn looks_like_deps_failure(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    const SIGNS: &[&str] = &[
        "could not resolve host",
        "could not resolve hostname",
        "temporary failure in name resolution",
        "network is unreachable",
        "no route to host",
        "connection refused",
        "connection timed out",
        "unable to access",
        "failed to fetch",
        "could not fetch",
        "could not read from remote repository",
        "rpc failed",
    ];
    SIGNS.iter().any(|sign| lowered.contains(sign))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = Cowboy;
        assert_eq!(w.id(), "cowboy");
        assert_eq!(w.category(), Category::Io);
        assert_eq!(w.contracts(), &["A.1", "A.6"]);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
    }

    #[test]
    fn probe_is_honest_about_a_missing_repo() {
        let p = Cowboy.probe();
        if p.reason() == Some("repo-missing") {
            assert!(!repo().is_dir());
        }
    }

    #[test]
    fn probe_no_longer_requires_a_load_generator() {
        // With the repo and `make` present the probe must be ready: there is no
        // external load-generator prerequisite any more.
        if repo().is_dir() && proc::which("make").is_some() {
            assert!(Cowboy.probe().is_ready());
        }
    }

    #[test]
    fn tail_truncates_and_keeps_the_end() {
        assert_eq!(tail("  hello  ", 10), "hello");
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("abc", 3), "abc");
        // Multi-byte safety: never split a char.
        assert_eq!(tail("aé€x", 2), "€x");
    }

    #[test]
    fn deps_failure_detection_matches_network_signs_not_compile_errors() {
        assert!(looks_like_deps_failure(
            "fatal: unable to access 'https://github.com/x/y': Could not resolve host: github.com"
        ));
        assert!(looks_like_deps_failure("curl: (7) Failed to fetch repo"));
        assert!(!looks_like_deps_failure(
            "src/hello_world_handler.erl:12: syntax error before: '->'"
        ));
    }

    #[test]
    fn find_release_script_returns_none_without_a_release() {
        let dir = std::env::temp_dir().join(format!("bench-cowboy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        assert_eq!(find_release_script(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_release_script_prefers_the_documented_path() {
        let dir = std::env::temp_dir().join(format!("bench-cowboy-rel-{}", std::process::id()));
        let bin = dir.join("_rel/hello_world_example/bin");
        std::fs::create_dir_all(&bin).expect("create bin dir");
        let script = bin.join("hello_world_example");
        std::fs::write(&script, "#!/bin/sh\n").expect("write script");
        assert_eq!(find_release_script(&dir), Some(script));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
