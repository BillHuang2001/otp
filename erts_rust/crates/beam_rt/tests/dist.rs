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

//! The **distributed node** (contract A.5, Phase 1) at the runtime boundary.
//!
//! What these tests pin, and with what:
//!
//! * a configured `-sname`/`-name` really binds a listener and really publishes the
//!   node to `epmd` (`beam_dist::epmd::lookup_port` resolves the *same* port the
//!   node reports) — skipped **loudly** when no `epmd` daemon is reachable;
//! * a **real system `erl` peer** completes the `'N'` handshake against this
//!   runtime: the peer is started with `net_kernel:connect_node/1`, which is real
//!   OTP resolving this node through `epmd` and performing the v6 handshake as the
//!   connecting side, and the runtime's own link reaches
//!   `DistConnection::is_established()` with the peer's full `node@host` name,
//!   while the peer reports `true` — skipped **loudly** when the system toolchain
//!   is absent;
//! * **the same path through the real executable**: the `beam_rt` binary runs as a
//!   `-sname` node (kept alive by the `run` action's scheduler pump, driven by a
//!   test-time-compiled keep-alive fixture whose loop parks on a `receive … after 25`),
//!   publishes itself to `epmd`, and a real system `erl` peer resolves and connects to
//!   it (`net_kernel:connect_node/1` → `true`) — see
//!   [`a_real_erl_peer_connects_to_the_running_binary_node`].
//!
//! The handshake only completes because the node advertises its **full**
//! `node@host` name on the wire: OTP's connecting side validates the name in the
//! server's `'N'`/challenge message against the node it dialled
//! (`dist_util.erl`'s `recv_challenge_new/2`), so a bare `-sname` advertised bare
//! is aborted mid-handshake (the proof on the protocol side is
//! `beam_dist/tests/live_accept.rs::a_bare_advertised_name_is_aborted_by_the_real_peer`).
//!
//! Phase 1 deliberately stops at `Established`: what a link carries afterwards
//! (`DOP_REG_SEND`/`DOP_SEND` into a mailbox) is Phase 2, so these tests assert the
//! handshake, the peer identity and the registration — never a delivered payload.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use beam_rt::{Runtime, RuntimeConfig};

/// How long a real `erl` peer is given to resolve and handshake with this node.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the real executable's node is given to publish itself to `epmd`.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the bounded CLI invocations (the brief keep-alive run) may take.
const CLI_TIMEOUT: Duration = Duration::from_secs(20);

/// The **real** executable under test (the test target's build guarantees it is
/// current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
/// A unique-enough tag for this test run's node/cookie names (the differential
/// corpus runs sequentially, but a developer may run the suite twice at once).
fn unique(tag: &str) -> String {
    format!("beamrt{}{}", tag, std::process::id())
}

/// Runs `erl -noshell -eval EXPR` and returns its trimmed stdout.
fn erl_eval(expr: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", expr])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The short hostname Erlang itself derives — the host part of a `-sname` node
/// name, so the peer's view of this node is in the node's own realm.
fn erl_hostname() -> Option<String> {
    let host = erl_eval("io:format(\"~s\",[element(2,inet:gethostname())]), halt().")?;
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Whether a real `erl` can be executed at all (no toolchain ⇒ skip loudly).
fn erl_available() -> bool {
    erl_eval("halt(0).").is_some()
}

/// Distinguishes "epmd answered `noport`" (reachable) from "no daemon" — the peer
/// crate returns a typed error for both, so the probe is its own connect.
fn epmd_unreachable() -> bool {
    match beam_dist::epmd::connect(beam_dist::EPMD_DEFAULT_ADDR) {
        Ok(_) => false,
        Err(_) => true,
    }
}

/// Starts a `-sname` runtime with `-setcookie` through the **CLI parser**, so the
/// configuration is exactly what the command line produces.
fn node_runtime(name: &str, cookie: &str) -> Runtime {
    let args: Vec<String> = ["-noshell", "-sname", name, "-setcookie", cookie, "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config: RuntimeConfig = beam_rt::parse(&args).expect("the node switches parse");
    Runtime::new(config).expect("the runtime starts (the listener binds)")
}

/// A real `erl` peer is spawned, resolves this node through `epmd` and completes
/// the `'N'` handshake against the runtime's listener.
struct Peer {
    child: Child,
    /// The peer's stdout, completed when the reader thread sees EOF (the peer
    /// halts on its own) or when the peer is killed.
    stdout: Receiver<String>,
}

impl Peer {
    /// Spawns `erl -sname peer -setcookie cookie` asking `net_kernel` to connect to
    /// `target` (a full `node@host` name). The peer prints the `~p` of its
    /// `net_kernel:connect_node/1` result and then stays alive for two seconds
    /// before halting, so the link is not torn down before this side observed it.
    /// Drop kills it.
    fn connect(name: &str, cookie: &str, target: &str) -> std::io::Result<Peer> {
        let eval = format!(
            "T = net_kernel:connect_node('{target}'), \
             io:format(\"~p\",[T]), \
             timer:sleep(2000), \
             halt().",
            target = target
        );
        let mut child = Command::new("erl")
            .arg("-sname")
            .arg(name)
            .arg("-setcookie")
            .arg(cookie)
            .args(["-noshell", "-eval", &eval])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("the peer's stdout is piped");
        // A detached reader: the peer's output is complete at EOF, which happens
        // when the peer halts by itself (or when Drop kills it).
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            let _ = tx.send(text);
        });
        Ok(Peer {
            child,
            stdout: rx,
        })
    }

    /// The peer's stdout, bounded by `timeout` (the peer halts on its own after
    /// two seconds, so this never waits longer than that unless `erl` hangs).
    fn stdout_within(&self, timeout: Duration) -> Option<String> {
        match self.stdout.recv_timeout(timeout) {
            Ok(text) => Some(text),
            Err(RecvTimeoutError::Timeout) => None,
            // The reader thread is gone without sending: the pipe is broken.
            Err(RecvTimeoutError::Disconnected) => Some(String::new()),
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `-sname NAME` makes the runtime a node: the name is `<NAME>@<host>`, the
/// listener is bound, and the node is published to `epmd` under the **short** name
/// (what a `PORT2_REQ` looks up).
#[test]
fn a_sname_node_binds_a_listener_and_registers_with_epmd() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable system `erl` (cannot derive the short hostname)");
        return;
    };
    let name = unique("n");
    let cookie = unique("c");
    let mut runtime = node_runtime(&name, &cookie);

    // One pass is what publishes the node (registration happens on the first pump).
    runtime.run_one_slice().expect("a scheduler pass");

    let node = runtime.dist_node().expect("a configured node is a DistNode");
    assert_eq!(node.node_name(), format!("{name}@{host}"));
    assert!(
        node.is_short_name(),
        "a bare `-sname` value is a short name: {}",
        node.node_name()
    );
    assert_eq!(
        node.epmd_name(),
        name.as_bytes(),
        "epmd is told the short name"
    );
    assert!(node.port() > 0, "the listener bound a real port");

    if epmd_unreachable() {
        eprintln!("SKIP (registration): no `epmd` daemon on {}", beam_dist::EPMD_DEFAULT_ADDR);
        return;
    }
    assert!(
        node.is_registered(),
        "epmd accepted the registration: {:?}",
        node.registration_error()
    );
    assert_eq!(
        beam_dist::epmd::lookup_port(name.as_bytes()).expect("the node is registered"),
        node.port(),
        "epmd resolves the name to the very port the listener bound"
    );
    assert!(node.is_alive(), "a registered, listening node is alive");
}

/// A **real system `erl`** resolves this node through `epmd` and handshakes with
/// it: the runtime's link reaches `Established` and recorded the peer's name, and
/// the peer's own `net_kernel:connect_node/1` answers `true`.
#[test]
fn a_real_erl_peer_completes_the_handshake_against_the_node() {
    if !erl_available() {
        eprintln!("SKIP: no usable system `erl`");
        return;
    }
    if epmd_unreachable() {
        eprintln!("SKIP: a real peer resolves this node through epmd, and no `epmd` is reachable");
        return;
    }
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: cannot derive the short hostname");
        return;
    };
    let name = unique("h");
    let peer_name = unique("p");
    let cookie = unique("k");
    let mut runtime = node_runtime(&name, &cookie);
    runtime.run_one_slice().expect("a scheduler pass");
    let target = runtime
        .node_name()
        .expect("the node's name")
        .to_string();
    assert_eq!(target, format!("{name}@{host}"));

    let peer = Peer::connect(&peer_name, &cookie, &target).expect("the peer node starts");

    // The link must reach `Established` with the connecting peer's name while the
    // peer is still alive (it stays up for two seconds after connecting).
    let deadline = Instant::now() + PEER_TIMEOUT;
    let mut established = None;
    while Instant::now() < deadline {
        runtime.run_scheduler().expect("a scheduler pass");
        if let Some(link) = runtime
            .dist_node()
            .expect("the node")
            .connections()
            .iter()
            .find(|link| link.is_established())
        {
            established = Some(link.peer_name());
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let node = runtime.dist_node().expect("the node");
    assert!(
        node.is_registered(),
        "the node is published to epmd (a peer can resolve it): {:?}",
        node.registration_error()
    );
    let Some(established_name) = established else {
        panic!(
            "a real erl peer resolved {target} through epmd but the 'N' handshake did not \
             complete within {PEER_TIMEOUT:?} (links={}, last_link_error={:?})",
            node.connections().len(),
            node.last_link_error(),
        );
    };
    // The `'N'` message carries the peer's own **full** `node@host` name (OTP's
    // `send_name/4` puts `atom_to_binary(Node)` on the wire), not a bare short name:
    // the same rule this node now follows for its own advertised name.
    assert_eq!(
        established_name.as_deref(),
        Some(format!("{peer_name}@{host}").as_str()),
        "the connecting peer's node name is what the 'N' message carried"
    );

    // The peer's own verdict: a real OTP node considers the distribution link up.
    // It prints the `~p` of `net_kernel:connect_node/1` and halts (bounded).
    let printed = peer
        .stdout_within(PEER_TIMEOUT)
        .expect("the peer prints its connect result and exits");
    assert_eq!(
        printed.trim(),
        "true",
        "a real erl node connected to {target} (peer printed {printed:?}, \
         last_link_error={:?})",
        node.last_link_error(),
    );
}

// -------------------------------------------------------------------------------------------
// The whole A.5 path through the real executable
// -------------------------------------------------------------------------------------------

/// The keep-alive fixture, compiled at test time with the system `erlc` (never committed).
///
/// `loop/0` parks the process on a `receive` whose `after 25` **arms a deadline**, and
/// repeats that 1200 times (≈ 30 s, comfortably under `Runtime::RUN_DEADLINE`'s 60 s):
/// each iteration leaves the runtime with a pending timer deadline, so the `run`
/// action's scheduler pump (`Runtime::run_scheduled`) keeps stepping — which is what
/// keeps the node up and its distribution half (`pump_dist`) polling. The `after` is
/// **load-bearing**: a bare `receive` with no `after` leaves the run queue, the ports
/// *and* the timer heap empty, so the pump's quiescence test holds and `run` reports its
/// honest `blocked` diagnostic instead (pinned by
/// [`a_receive_after_deadline_carries_a_looping_process_to_its_return`]).
///
/// `brief/0` is the same loop four iterations long (≈ 100 ms), so the deadline that
/// makes the fixture work is also pinned quickly and at the CLI boundary.
const KEEPALIVE_ERL: &str = r#"%% Generated by `beam_rt/tests/dist.rs` (never committed).
-module(node_keepalive).
-export([loop/0, brief/0]).

loop() -> loop(1200).
brief() -> loop(4).

loop(0) -> done;
loop(N) -> receive _ -> ok after 25 -> ok end, loop(N - 1).
"#;
/// The keep-alive fixture's module name (must be what the `run` spec names).
const KEEPALIVE_MODULE: &str = "node_keepalive";

/// The long, keep-alive entry point: the node stays up for ≈ 30 s.
const KEEPALIVE_SPEC: &str = "node_keepalive:loop/0";
/// The short entry point: the same loop, ≈ 100 ms.
const BRIEF_SPEC: &str = "node_keepalive:brief/0";

/// A scratch directory holding a test-time-compiled fixture, removed on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// Creates `<tmp>/beam_rt_node_<tag>_<pid>`, or `None` (**loudly**) when the
    /// temporary directory cannot be made.
    fn new(tag: &str) -> Option<Scratch> {
        let dir = std::env::temp_dir().join(format!("beam_rt_node_{tag}_{}", std::process::id()));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => Some(Scratch { dir }),
            Err(error) => {
                eprintln!(
                    "SKIP: cannot create the scratch directory {}: {error}",
                    dir.display()
                );
                None
            }
        }
    }

    /// The scratch directory itself (the `erlc` output directory).
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Compiles [`KEEPALIVE_ERL`] as `<dir>/<KEEPALIVE_MODULE>.beam` with the system
/// `erlc`, or `None` when the compiler is missing or fails (**loudly**) — the same skip
/// policy as `tests/driver.rs::compile_erl`.
fn compile_keepalive(dir: &Path) -> Option<PathBuf> {
    let source = dir.join(format!("{KEEPALIVE_MODULE}.erl"));
    if let Err(error) = std::fs::write(&source, KEEPALIVE_ERL) {
        eprintln!("SKIP: cannot write {}: {error}", source.display());
        return None;
    }
    let output = match Command::new("erlc").arg("-o").arg(dir).arg(&source).output() {
        Ok(output) => output,
        Err(error) => {
            eprintln!("SKIP: cannot run `erlc` ({error})");
            return None;
        }
    };
    if !output.status.success() {
        eprintln!(
            "SKIP: `erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let beam = dir.join(format!("{KEEPALIVE_MODULE}.beam"));
    if !beam.is_file() {
        eprintln!("SKIP: `erlc` produced no {}", beam.display());
        return None;
    }
    Some(beam)
}

/// The **real `beam_rt` executable** running as a `-sname` node, kept alive by the
/// keep-alive fixture — a RAII handle whose `Drop` kills and reaps the process (on panic
/// too). The caller keeps the [`Scratch`] holding the fixture alive; it is removed after
/// this handle has been dropped.
struct NodeChild {
    child: Child,
    /// Where the node's stderr is captured (its diagnostics, read only on failure).
    stderr_path: PathBuf,
    /// The exact command line, so a failure names what was run.
    command: String,
}

impl NodeChild {
    /// Spawns `BINARY -noshell -sname NAME -setcookie COOKIE run FIXTURE KEEPALIVE_SPEC`,
    /// with stdout discarded and stderr captured into `scratch`.
    fn spawn(
        name: &str,
        cookie: &str,
        scratch: &Path,
        fixture: &Path,
    ) -> std::io::Result<NodeChild> {
        let stderr_path = scratch.join("node-stderr.txt");
        let args: Vec<String> = vec![
            "-noshell".into(),
            "-sname".into(),
            name.into(),
            "-setcookie".into(),
            cookie.into(),
            "run".into(),
            fixture.display().to_string(),
            KEEPALIVE_SPEC.into(),
        ];
        let command = format!("{BINARY} {}", args.join(" "));
        let child = Command::new(BINARY)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr_path)?))
            .spawn()?;
        Ok(NodeChild {
            child,
            stderr_path,
            command,
        })
    }
    /// Whether the node process has already exited (a fail-fast probe, so a broken node
    /// is reported at once instead of after the poll times out).
    fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// The node's stderr so far (empty when it has said nothing).
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    /// The exact command line the node was started with.
    fn command(&self) -> &str {
        &self.command
    }
}

impl Drop for NodeChild {
    fn drop(&mut self) {
        // Kill and reap the node (on an assertion panic too), so it never outlives the
        // test that started it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}/// Runs `BINARY args…`, returning its exit status and stdout. **Bounded**: the child is
/// killed and `None` is returned when `timeout` expires, so a regression is a loud
/// failure, never a hang.
fn run_binary_within(args: &[String], timeout: Duration) -> (Option<ExitStatus>, String) {
    let mut child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn {BINARY}: {error}"));
    let mut stdout = child.stdout.take().expect("the child's stdout is piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = tx.send(text);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().expect("the child is waitable") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    // The reader sees EOF once the child is gone; the small bound only protects against a
    // grandchild holding the pipe open.
    let output = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    (status, output)
}

/// **The whole A.5 path through the real executable.** The `beam_rt` binary runs as a
/// `-sname` node kept alive by the `run` action's scheduler pump (the keep-alive
/// fixture's deadline-driven loop), publishes itself to `epmd`, and a real system `erl`
/// peer resolves it through `epmd` and completes the `'N'` handshake as the connecting
/// side: `net_kernel:connect_node/1` answers **`true`**.
///
/// Skipped **loudly** without a system `erl`/`erlc` or a reachable `epmd`.
#[test]
fn a_real_erl_peer_connects_to_the_running_binary_node() {
    if !erl_available() {
        eprintln!("SKIP: no usable system `erl`");
        return;
    }
    if epmd_unreachable() {
        eprintln!("SKIP: a real peer resolves this node through epmd, and no `epmd` is reachable");
        return;
    }
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: cannot derive the short hostname");
        return;
    };
    let Some(scratch) = Scratch::new("node") else {
        return;
    };
    let Some(fixture) = compile_keepalive(scratch.path()) else {
        return;
    };

    let name = unique("rn");
    let peer_name = unique("rp");
    let cookie = unique("rc");
    let mut node = NodeChild::spawn(&name, &cookie, scratch.path(), &fixture)
        .unwrap_or_else(|error| panic!("cannot spawn the beam_rt node binary ({BINARY}): {error}"));
    // The `run` pump publishes the node to `epmd` on its first scheduler pass, and the
    // keep-alive fixture's deadline keeps that pump stepping, so the name must appear —
    // this is exactly how `a_sname_node_binds_a_listener_and_registers_with_epmd` proves
    // registration (bounded, and it fails fast when the node has already died).
    let deadline = Instant::now() + REGISTER_TIMEOUT;
    let mut registered = None;
    while Instant::now() < deadline {
        if let Ok(port) = beam_dist::epmd::lookup_port(name.as_bytes()) {
            registered = Some(port);
            break;
        }
        if node.has_exited() {
            panic!(
                "the beam_rt node exited before it registered with epmd\n  command: {}\n  stderr: {}",
                node.command(),
                node.stderr(),
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let Some(port) = registered else {
        panic!(
            "the beam_rt node never appeared in epmd within {REGISTER_TIMEOUT:?}\n  command: {}\n  stderr: {}",
            node.command(),
            node.stderr(),
        );
    };
    assert!(port > 0, "epmd resolved the node's listener port");

    // A real OTP node resolves `<NAME>@<host>` through epmd and handshakes with it as the
    // connecting side; its own verdict is the `~p` of `net_kernel:connect_node/1`.
    let target = format!("{name}@{host}");
    let peer = Peer::connect(&peer_name, &cookie, &target).expect("the peer node starts");
    let printed = peer.stdout_within(PEER_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "the real `erl` peer did not report `net_kernel:connect_node('{target}')` within \
             {PEER_TIMEOUT:?} (node alive: {}, node stderr: {})",
            !node.has_exited(),
            node.stderr(),
        )
    });
    assert_eq!(
        printed.trim(),
        "true",
        "a real erl node connected to the running beam_rt node {target} via epmd port {port} \
         (the peer printed {printed:?}, node stderr: {})",
        node.stderr(),
    );
    // The handshake happened while the keep-alive loop was still parked on its deadline:
    // the process is still alive, not finished (its `x(0)` would be `done`).
    assert!(
        !node.has_exited(),
        "the keep-alive loop still holds the node up after the handshake"
    );
}

/// The keep-alive loop's `after 25` is **load-bearing**, and the pump carries the parked
/// process to its `return` through it: at the CLI boundary the deadline-driven loop runs
/// to `loop(0) -> done` (exit 0, stdout `done`).
///
/// The contrast is the point — a bare `receive` with no `after` leaves the run queue, the
/// ports and the timer heap empty, so `Runtime::is_quiescent` holds and `run` exits 3
/// with its `unsupported: opcode: run: process blocked (no message available)`
/// diagnostic. So this test fails if the fixture's deadline is ever removed.
#[test]
fn a_receive_after_deadline_carries_a_looping_process_to_its_return() {
    let Some(scratch) = Scratch::new("brief") else {
        return;
    };
    let Some(fixture) = compile_keepalive(scratch.path()) else {
        return;
    };
    let args: Vec<String> = vec![
        "-noshell".into(),
        "run".into(),
        fixture.display().to_string(),
        BRIEF_SPEC.into(),
    ];
    let (status, stdout) = run_binary_within(&args, CLI_TIMEOUT);
    let Some(status) = status else {
        panic!(
            "`{BINARY} {}` did not terminate within {CLI_TIMEOUT:?}",
            args.join(" ")
        );
    };
    assert_eq!(
        status.code(),
        Some(0),
        "the deadline-driven loop terminates normally (stdout: {stdout:?})"
    );
    assert_eq!(
        stdout.trim(),
        "done",
        "the parked loop resumed on its `after` deadline each iteration and reached `loop(0) -> done`"
    );
}