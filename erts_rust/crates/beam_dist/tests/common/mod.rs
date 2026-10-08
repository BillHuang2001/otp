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

#![allow(dead_code)]
#![deny(unsafe_code)]

//! Shared harness for the crate's **live** integration tests
//! ([`../live_interop.rs`], [`../live_driver.rs`]).
//!
//! Both live tests spawn a real `erl` node, read its stdout/stderr on background
//! threads, resolve its distribution listener and reap the child on drop. That
//! machinery is identical, so it lives here once (the crate layer's
//! Prevent-Duplication rule); each test supplies only its own Erlang script and
//! its own wire-level expectations.
//!
//! `#![allow(dead_code)]` is deliberate: this module is compiled into *both* test
//! binaries and neither uses every helper.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How long to wait for the node to print its readiness line.
pub const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to keep retrying the TCP connect to the node's listener.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The socket read timeout for an established link (bounds every blocking `recv`).
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for the node to print a message it received.
pub const MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to keep polling `epmd` for the node's registration.
pub const EPMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Which route produced a node's distribution listen port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortSource {
    /// `epmd` (`PORT2_REQ`/`PORT2_RESP`) resolved the node's registered port.
    Epmd,
    /// `epmd` was unreachable or did not know the node; the pinned port is used.
    Fallback,
}

/// A spawned `erl` node whose stdout/stderr are read on background threads.
///
/// Killing and reaping the child (and joining the readers) happens in [`Drop`], so
/// a failing assertion never leaks the node.
pub struct ErlangNode {
    child: Child,
    lines: Receiver<String>,
    stdout_thread: Option<thread::JoinHandle<()>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
    port: u16,
}

impl ErlangNode {
    /// Starts a `-sname` node listening on `127.0.0.1:port` and evaluating
    /// `eval` in the context of a `-noshell -eval` startup.
    ///
    /// The listen port is pinned with `inet_dist_listen_min`/`inet_dist_listen_max`,
    /// so the node registers exactly that port with `epmd`.
    pub fn start(name: &str, cookie: &str, port: u16, eval: &str) -> std::io::Result<ErlangNode> {
        let mut child = Command::new("erl")
            .arg("-sname")
            .arg(name)
            .arg("-setcookie")
            .arg(cookie)
            .arg("-kernel")
            .arg("inet_dist_listen_min")
            .arg(port.to_string())
            .arg("inet_dist_listen_max")
            .arg(port.to_string())
            .arg("-noshell")
            .arg("-eval")
            .arg(eval)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        let (tx, lines) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    // Stop if the receiver (the test) has gone away.
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        let stderr_thread = thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                match line {
                    Ok(line) => {
                        if let Ok(mut buf) = stderr_sink.lock() {
                            buf.push_str(&line);
                            buf.push('\n');
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(ErlangNode {
            child,
            lines,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr,
            port,
        })
    }

    /// The pinned loopback port of the node's distribution listener.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The loopback address of the node's distribution listener.
    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Waits (bounded by `timeout`) for a stdout line satisfying `pred`.
    ///
    /// Returns the matching line, or `None` if the timeout elapses first (or the
    /// node's stdout closes).
    pub fn wait_for<F: Fn(&str) -> bool>(&self, pred: F, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    if pred(&line) {
                        return Some(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// The node's stderr collected so far (for failure diagnostics).
    pub fn stderr_text(&self) -> String {
        self.stderr.lock().map(|buf| buf.clone()).unwrap_or_default()
    }
}

impl Drop for ErlangNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Reports whether a usable `erl` exists and, if so, the short hostname Erlang
/// itself derives. That hostname is the host part of a `-sname` node name, so
/// deriving it here (rather than from the OS) guarantees the client node name is
/// in the node's own realm. `None` means "skip".
///
/// `inet:gethostname/0` returns `{ok, Host}` on OTP 29, hence the `element/2`.
pub fn erl_hostname() -> Option<String> {
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[element(2,inet:gethostname())]), halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// A process-unique run id, so a run's node name/cookie never collide with those of
/// a node spawned by a concurrent test in the same binary.
pub fn run_id() -> u32 {
    static RUN: AtomicU32 = AtomicU32::new(0);
    RUN.fetch_add(1, Ordering::Relaxed)
}

/// Picks a free loopback TCP port by binding an ephemeral listener and dropping
/// it (a small TOCTOU window that the connect retry loop absorbs).
pub fn free_port() -> u16 {
    for _ in 0..16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
        let port = listener.local_addr().expect("read the ephemeral port").port();
        drop(listener);
        if port != 0 {
            return port;
        }
    }
    panic!("could not obtain a free loopback port");
}

/// Resolves the distribution listen port of the `-sname` node registered as
/// `node_name` (the **short** name, as `PORT2_REQ` requires) through `epmd`.
///
/// The lookup is retried until `timeout` elapses, because a node registers with
/// `epmd` asynchronously during startup. When `epmd` is unreachable (or does not
/// know the name) the pinned `fallback` port is returned instead, together with
/// the last `epmd` error text — the caller reports which route it used rather than
/// failing, since the node's listener is a fixed, known port either way.
pub fn resolve_listen_port(
    node_name: &str,
    fallback: u16,
    timeout: Duration,
) -> (u16, PortSource, Option<String>) {
    let deadline = Instant::now() + timeout;
    loop {
        let error = match beam_dist::epmd::lookup_port(node_name.as_bytes()) {
            Ok(port) => return (port, PortSource::Epmd, None),
            Err(err) => format!("{err:?}"),
        };
        if Instant::now() >= deadline {
            return (fallback, PortSource::Fallback, Some(error));
        }
        thread::sleep(Duration::from_millis(100));
    }
}
