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

//! **Inbound distribution** (contract A.5, phase 2) against a **real system `erl` peer**.
//!
//! [`tests/dist.rs`](../dist.rs) pins the *accepting* half of a distribution link at the
//! handshake boundary (a real `erl` resolves this node through `epmd`, dials it and
//! completes the `'N'` handshake), and [`tests/dist_client.rs`](../dist_client.rs) pins
//! the mirror image (this runtime dials a real peer and `DOP_REG_SEND`s it a message —
//! the **outbound** direction). This suite closes the loop: a real `erl` peer *sends*
//! this runtime a message, and the message is delivered into the mailbox of a **running
//! local process**, observable as that process's return value.
//!
//! The shape of the interaction:
//!
//! 1. this runtime is started as a `-sname` node, and its first scheduler pass publishes
//!    it to `epmd` (the listening half of [`crate::dist`]);
//! 2. a real `erl -sname` peer polls `net_kernel:connect_node('<our node>')` until it
//!    answers `true` — that is a real OTP node resolving our listener through `epmd` and
//!    completing the `'N'` handshake **as the connecting side** (so our runtime is the
//!    acceptor, `ConnectionDriver::server`). (`net_adm:ping/1` is deliberately not the
//!    probe: it is a `gen:call` to the remote `net_kernel`, which this runtime does not
//!    run — see [`Peer::start`].)
//! 3. the fixture [`dist_echo:wait/0`](fixtures/dist_echo.erl) registers the name `echo`
//!    and blocks on a `receive`;
//! 4. the peer sends `{echo, '<our node>'} ! {hello, node()}` — a `DOP_REG_SEND` on the
//!    established link, and then prints the message it gets back, if any;
//! 5. our runtime's scheduler pass decodes the control message
//!    ([`Runtime::pump_dist`](../src/dist.rs) + `dispatch_dist_inbound`) and routes it
//!    through [`LocalDeliverySink`](../src/dist.rs) — a `DOP_REG_SEND` resolved against
//!    the local name registry — into the blocked process's mailbox, waking it; the
//!    `receive` then matches and the fixture returns the delivered term.
//!
//! The assertion is therefore on the **delivered term** (in-process: the value
//! `Runtime::run_scheduled` returns; at the CLI boundary: the `~w~n` the `run` action
//! prints), never on this runtime's belief about what a peer sent.
//!
//! Every test here is a **loud skip** (`SKIP: …` + early `return`, never a panic and
//! never a silent pass) when no usable `erl`/`epmd` is available, and the peer is killed
//! and reaped on drop.
//!
//! # What is *not* proven here
//!
//! The peer here is the **connecting** side, so the delivery rides the socket *it*
//! opened (our listener link). The `DOP_SEND_SENDER` **sender-bearing** envelope — a
//! mailbox `from` word derived from an external-pid sender on that same link — needs the
//! peer to address one of *our* pids, which requires our runtime to send the peer that
//! pid first; the runtime exposes a send only on the **outbound** (connector) link
//! (`Runtime::dist_send_message_pid`), so exercising it belongs with the connector suite
//! and is deliberately out of scope here (see the module handoff note on
//! [`tests/dist_client.rs`](../dist_client.rs)).

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use beam_rt::{Runtime, RuntimeConfig};
use beam_term::heap::Heap;

// The shared harness is compiled into every test target; this target only uses a
// subset of it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

/// How long our own node may take to appear in `epmd` (registration is asynchronous
/// inside the node's first scheduler pass).
const EPMD_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the peer is given to print the message it got back (if any).
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the CLI `run` invocation under test may take before it is killed.
const CLI_TIMEOUT: Duration = Duration::from_secs(45);

/// The committed inbound-distribution fixture (`erlc +deterministic dist_echo.erl`,
/// recipe in `tests/fixtures/README.md`). It is passed to the runtime by **path**
/// (`Runtime::run_scheduled`/the `run` action), never through a `-pa` on the fixtures
/// directory — that directory holds a `lists.beam` which would shadow stdlib's.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dist_echo.beam");

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// A process-unique tag, so a run's node names/cookie never collide with a concurrent
/// test's (or a leftover node from an earlier run).
fn unique(tag: &str) -> String {
    format!("beamrtli{}{}", tag, std::process::id())
}

/// The **oracle** `erl` to spawn as the peer, resolved without running it; a missing
/// toolchain is the shared loud skip ([`support::require_oracle`]).
fn oracle() -> Option<PathBuf> {
    support::require_oracle("live_inbound")
}

/// The short hostname Erlang itself derives — the host part of a `-sname` node name,
/// so both nodes live in the same realm. This runtime derives its own host the same way
/// (`dist::local_hostname` reads `/proc/sys/kernel/hostname`), which the existing
/// `tests/dist.rs`/`tests/dist_client.rs` rely on too.
fn erl_hostname(oracle: &Path) -> Option<String> {
    let output = support::run_bounded(
        oracle,
        &[
            "-noshell",
            "-eval",
            "io:format(\"~s\",[element(2,inet:gethostname())]), halt().",
        ],
    )?;
    let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!host.is_empty() && output.status.success()).then_some(host)
}

/// Whether a `beam_dist` client can reach `epmd` at all (no daemon ⇒ the peer can
/// neither register nor be resolved ⇒ loud skip).
fn epmd_unreachable() -> bool {
    beam_dist::epmd::connect(beam_dist::EPMD_DEFAULT_ADDR).is_err()
}

/// Starts a `-sname` runtime through the **CLI parser**, so its configuration is
/// exactly what a command line produces. Boots the preloaded modules and binds the
/// distribution listener.
fn node_runtime(name: &str, cookie: &str) -> Option<Runtime> {
    let args: Vec<String> = ["-noshell", "-sname", name, "-setcookie", cookie, "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config: RuntimeConfig = beam_rt::parse(&args).ok()?;
    Runtime::new(config).ok()
}

/// A real `erl` node acting as the **connecting** peer: it polls
/// `net_kernel:connect_node/1` for `target` until it answers `true`, then sends
/// `{echo, Target} ! {hello, node()}` and prints what it received.
///
/// `lines` yields the peer's stdout one line at a time; `stderr` accumulates its
/// diagnostics for failure messages. Drop kills and reaps it.
struct Peer {
    child: Child,
    lines: Receiver<String>,
    transcript: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    command: String,
}

impl Peer {
    /// Spawns `erl -sname <node> -setcookie <cookie> -noshell -eval <EXPR>` where `EXPR`
    /// polls for `target`, sends the greeting and prints every line it produces.
    ///
    /// The readiness probe is `net_kernel:connect_node/1` — a real OTP node resolving our
    /// listener through `epmd` and driving the `'N'` handshake as the **connecting**
    /// side (exactly what [`tests/dist.rs`](../dist.rs) uses). `net_adm:ping/1` is *not*
    /// usable here: it is a `gen:call` to the **remote `net_kernel`** (`{is_auth, node()}`,
    /// `kernel/src/net_adm.erl:101-110`) with an `infinity` timeout, and this runtime
    /// runs no `net_kernel`/auth server, so it would block for ever rather than answer
    /// `pang`. That is a real capability asymmetry, named here rather than papered over.
    fn start(oracle: &Path, node: &str, cookie: &str, target: &str) -> std::io::Result<Peer> {
        // The peer connects, then `DOP_REG_SEND`s the greeting to our registered name.
        // `node()` is the peer's own full `node@host` name, which is exactly what the
        // delivered payload carries back.
        let eval = format!(
            "Target = list_to_atom(\"{target}\"), \
             Ping = fun F(0) -> io:format(\"CONNFAIL~n\",[]), halt(2); \
                        F(N) -> case net_kernel:connect_node(Target) of \
                                    true -> ok; \
                                    _ -> timer:sleep(100), F(N - 1) \
                                end \
                    end, \
             Ping(300), \
             io:format(\"CONNECTED ~p~n\", [node()]), \
             {{echo, Target}} ! {{hello, node()}}, \
             io:format(\"SENT ~p~n\", [node()]), \
             receive M -> io:format(\"BACK ~p~n\", [M]) after 8000 -> ok end, \
             halt().",
            target = target,
        );
        let mut child = Command::new(oracle)
            .arg("-sname")
            .arg(node)
            .arg("-setcookie")
            .arg(cookie)
            .args(["-noshell", "-eval", &eval])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().expect("the peer's stdout is piped");
        let stderr_pipe = child.stderr.take().expect("the peer's stderr is piped");
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&transcript);
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if let Ok(mut buf) = sink.lock() {
                            buf.push(line.clone());
                        }
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                match line {
                    Ok(line) => match sink.lock() {
                        Ok(mut buf) => {
                            buf.push_str(&line);
                            buf.push('\n');
                        }
                        Err(_) => break,
                    },
                    Err(_) => break,
                }
            }
        });

        Ok(Peer {
            child,
            lines,
            transcript,
            stderr,
            command: format!(
                "{oracle:?} -sname {node} -setcookie {cookie} -noshell -eval {eval}"
            ),
        })
    }

    /// Waits (bounded) for a stdout line satisfying `pred`, discarding earlier lines.
    fn wait_for<F: Fn(&str) -> bool>(&self, pred: F, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) if pred(&line) => return Some(line),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    return None
                }
            }
        }
    }

    /// Every stdout line the peer has printed so far (for failure diagnostics).
    fn transcript_text(&self) -> String {
        self.transcript
            .lock()
            .map(|buf| buf.join("\n"))
            .unwrap_or_default()
    }

    /// The peer's stderr collected so far (for failure diagnostics).
    fn stderr_text(&self) -> String {
        self.stderr.lock().map(|buf| buf.clone()).unwrap_or_default()
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Asserts the term `delivered` is the tuple `{hello, <peer_node>@<host>}` — the exact
/// payload the peer's `{echo, Target} ! {hello, node()}` puts on the wire.
///
/// The comparison is structural through the runtime's own atom table, so a delivered
/// atom only matches when it *is* the peer's node atom (not merely a like-named one).
fn assert_is_hello_from(
    rt: &mut Runtime,
    delivered: beam_term::Eterm,
    peer_node: &str,
    host: &str,
) {
    let peer_atom_name = format!("{peer_node}@{host}");
    let mut heap = Heap::new(64);
    let hello = {
        // `intern` takes `&mut Runtime`; the caller owns it, so re-derive through the
        // atom table the delivered term was built on.
        rt.atoms().lookup("hello").expect("`hello` is interned")
    };
    let peer_atom = rt
        .atoms()
        .lookup(&peer_atom_name)
        .unwrap_or_else(|| panic!("`{peer_atom_name}` is interned (the payload carries it)"));
    let expected = heap
        .tuple(&[beam_term::make_atom(hello), beam_term::make_atom(peer_atom)])
        .expect("the expected tuple");
    assert!(
        beam_term::compare::eq(delivered, expected, rt.atoms()),
        "the delivered term is `{{hello,{peer_atom_name}}}`; the runtime printed it as {:?}",
        rt.format_term(delivered, None).unwrap_or_default(),
    );
}

/// **The inbound direction, end to end.** A real `erl` peer resolves this runtime's
/// listener through `epmd`, handshakes into it, and its `DOP_REG_SEND` reaches the
/// running fixture process's mailbox — the process returns the delivered payload, which
/// this test asserts is exactly `{hello, <peer>@<host>}`.
#[test]
fn a_real_erl_peer_reg_sends_into_a_running_local_process() {
    let Some(oracle) = oracle() else {
        return; // `require_oracle` already reported the loud skip
    };
    if epmd_unreachable() {
        eprintln!(
            "SKIP: the peer resolves this node through `epmd`, and no daemon answers on {}",
            beam_dist::EPMD_DEFAULT_ADDR
        );
        return;
    }
    let Some(host) = erl_hostname(&oracle) else {
        eprintln!("SKIP: the system `erl` cannot report its own short hostname");
        return;
    };

    let our_node = unique("n");
    let peer_node = unique("p");
    let cookie = unique("k");

    let Some(mut rt) = node_runtime(&our_node, &cookie) else {
        panic!("the node runtime starts (it binds the distribution listener)");
    };
    // The first pass is what publishes this node to `epmd`, so a real peer can resolve it.
    rt.run_one_slice().expect("a scheduler pass");
    assert!(
        rt.is_alive(),
        "the node is published to epmd: {:?}",
        rt.dist_node().and_then(|n| n.registration_error())
    );
    let target = rt.node_name().expect("a configured node has a name").to_string();
    assert_eq!(
        target,
        format!("{our_node}@{host}"),
        "this runtime's node name is in the same realm as the system `erl`"
    );
    // The reachability evidence no peer can fake: `epmd` itself resolves the short name
    // to the very port this runtime's listener bound (registration happens inside the
    // scheduler pass above, so the lookup is polled briefly).
    let deadline = Instant::now() + EPMD_TIMEOUT;
    let mut resolved = None;
    while Instant::now() < deadline {
        if let Ok(port) = beam_dist::epmd::lookup_port(our_node.as_bytes()) {
            resolved = Some(port);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let Some(port) = resolved else {
        panic!(
            "this node never appeared in `epmd` within {EPMD_TIMEOUT:?}: {:?}",
            rt.dist_node().and_then(|n| n.registration_error())
        );
    };
    assert_eq!(
        Some(port),
        rt.dist_node().map(|n| n.port()),
        "epmd resolves our short name to the port our listener bound"
    );

    let peer = Peer::start(&oracle, &peer_node, &cookie, &target)
        .expect("the real `erl` peer starts");

    // Run the fixture as a **scheduled process**: this pumps the full scheduler — the
    // listener, the `epmd` registration, the accepted link's handshake and the inbound
    // delivery — until the peer's message arrives and the fixture returns it.
    let delivered = rt.run_scheduled(FIXTURE, "wait", 0, &[]).unwrap_or_else(|err| {
        panic!(
            "the fixture never received the peer's message: {err}\n  peer command: {}\n  \
             peer stdout: {:?}\n  peer stderr: {}",
            peer.command,
            peer.transcript_text(),
            peer.stderr_text()
        )
    });

    assert_is_hello_from(&mut rt, delivered, &peer_node, &host);

    // The reachability evidence: the peer's own report that it connected (it resolved us
    // through `epmd` and completed the handshake as the connecting side) …
    let connected = peer
        .wait_for(|line| line.contains("CONNECTED"), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the peer never connected to our node\n  peer command: {}\n  peer stdout: \
                 {:?}\n  peer stderr: {}",
                peer.command,
                peer.transcript_text(),
                peer.stderr_text()
            )
        });
    assert!(
        connected.contains(&format!("{peer_node}@{host}")),
        "the peer reported its own full node name (peer printed {connected:?})"
    );
    // … and this runtime's own link state: an accepted connection that reached
    // `Established` with the peer's full `node@host` name.
    let established = rt
        .dist_node()
        .expect("the node")
        .connections()
        .iter()
        .filter(|link| link.is_established())
        .map(|link| link.peer_name())
        .collect::<Vec<_>>();
    assert_eq!(
        established,
        vec![Some(format!("{peer_node}@{host}"))],
        "the peer's inbound link is established in this runtime (last link error: {:?})",
        rt.dist_node().and_then(|n| n.last_link_error()),
    );
}

/// Runs `BINARY args…`, returning its exit status and stdout. **Bounded**: the child is
/// killed and `None` is returned when `timeout` expires, so a regression is a loud
/// failure, never a hang.
fn run_binary_within(args: &[String], timeout: Duration) -> (Option<ExitStatus>, String) {
    let mut child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn {BINARY}: {error}"));
    let mut stdout = child.stdout.take().expect("the child's stdout is piped");
    let mut stderr = child.stderr.take().expect("the child's stderr is piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = tx.send(text);
    });
    let (etx, erx) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        let _ = etx.send(text);
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
    // The readers see EOF once the child is gone; the small bound only protects against a
    // grandchild holding the pipe open.
    let output = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    let err_text = erx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    if status.is_none() {
        eprintln!("--- {BINARY} stdout before kill ---\n{output}\n--- stderr ---\n{err_text}");
    }
    (status, output)
}

/// **The inbound direction at the CLI boundary.** The `beam_rt` binary runs as a
/// `-sname` node through the real `run` action, a real system `erl` peer resolves it
/// through `epmd` and sends it a `DOP_REG_SEND`, and the delivered payload is what the
/// `run` action prints as its result (`~w~n`).
#[test]
fn the_cli_node_receives_an_inbound_reg_send_from_a_real_erl_peer() {
    let Some(oracle) = oracle() else {
        return; // `require_oracle` already reported the loud skip
    };
    if epmd_unreachable() {
        eprintln!(
            "SKIP: the peer resolves this node through `epmd`, and no daemon answers on {}",
            beam_dist::EPMD_DEFAULT_ADDR
        );
        return;
    }
    let Some(host) = erl_hostname(&oracle) else {
        eprintln!("SKIP: the system `erl` cannot report its own short hostname");
        return;
    };

    let our_node = unique("c");
    let peer_node = unique("q");
    let cookie = unique("j");
    let target = format!("{our_node}@{host}");

    // The peer is started first: it polls our node (for up to 30 s) until the CLI node
    // publishes itself to `epmd`, then sends the greeting.
    let peer = Peer::start(&oracle, &peer_node, &cookie, &target)
        .expect("the real `erl` peer starts");

    let args: Vec<String> = vec![
        "-noshell".into(),
        "-sname".into(),
        our_node.clone(),
        "-setcookie".into(),
        cookie.clone(),
        "run".into(),
        FIXTURE.into(),
        "dist_echo:wait/0".into(),
    ];
    let (status, stdout) = run_binary_within(&args, CLI_TIMEOUT);
    let Some(status) = status else {
        panic!(
            "`{BINARY} {}` did not terminate within {CLI_TIMEOUT:?}\n  peer command: {}\n  \
             peer stdout: {:?}\n  peer stderr: {}",
            args.join(" "),
            peer.command,
            peer.transcript_text(),
            peer.stderr_text()
        );
    };

    // The peer's `CONNECTED` is the reachability evidence: it only prints after it
    // resolved this node through `epmd` and completed the `'N'` handshake. The line
    // carries the peer's own full `node@host` name.
    let connected = peer
        .wait_for(|line| line.contains("CONNECTED"), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the peer never connected to the CLI node\n  peer command: {}\n  peer stdout: \
                 {:?}\n  peer stderr: {}",
                peer.command,
                peer.transcript_text(),
                peer.stderr_text()
            )
        });
    assert!(
        connected.contains(&format!("{peer_node}@{host}")),
        "the peer connected (`{target}`) and reported its own node name (printed {connected:?})"
    );

    assert_eq!(
        status.code(),
        Some(0),
        "the `run` action exits 0 once the fixture returns the delivered message\n  \
         stdout: {stdout:?}\n  peer stderr: {}",
        peer.stderr_text(),
    );
    let expected = format!("{{hello,{peer_node}@{host}}}");
    assert!(
        stdout.trim() == expected,
        "the CLI printed the payload the peer's `DOP_REG_SEND` delivered (expected {expected:?}, \
         got {stdout:?}, peer stderr: {})",
        peer.stderr_text(),
    );
}

/// The fixture's executable bit / path sanity: a missing committed `.beam` must be a
/// loud failure, not an obscure runtime diagnostic in the tests above.
#[test]
fn the_fixture_beam_is_present() {
    assert!(
        Path::new(FIXTURE).is_file(),
        "the committed fixture {FIXTURE} is missing (regenerate per tests/fixtures/README.md)"
    );
}
