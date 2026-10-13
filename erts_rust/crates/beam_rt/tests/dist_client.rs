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

//! The **connecting (client) half** of distribution (contract A.5, phase 2) against a
//! **real system `erl` peer**.
//!
//! [`tests/dist.rs`](../dist.rs) pins the *listening* half: a real `erl` resolves this
//! runtime through `epmd` and handshakes *into* it. This suite pins the mirror image —
//! this runtime is the **connector**: it resolves the peer through `epmd`, drives the
//! client side of the `'N'` handshake to `Established`
//! ([`Runtime::dist_connect`](beam_rt::Runtime::dist_connect)) and then carries a real
//! `DOP_REG_SEND` to a **registered process on the peer**
//! ([`Runtime::dist_send_reg_send`](beam_rt::Runtime::dist_send_reg_send)), whose own
//! `io:format/2` of the payload is the observable evidence.
//!
//! The peer is a `-sname` node with its distribution listener pinned by
//! `inet_dist_listen_min`/`inet_dist_listen_max` (the recipe
//! `beam_dist/tests/common/mod.rs::ErlangNode::start` uses); its `-eval` registers a
//! name on `init` and prints every message it receives, so the assertion is on the
//! peer's own stdout — never on this runtime's belief about what it sent.
//!
//! Every test here is a **loud skip** (`SKIP: …` + early `return`, never a panic and
//! never a silent pass) when no usable `erl`/`epmd` is available, and the peer is
//! killed and reaped on drop.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use beam_rt::{Runtime, RuntimeConfig};
use beam_term::heap::Heap;
use beam_term::prelude::*;

mod support;

/// How long the peer is given to print its readiness line.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `epmd` is polled for the peer's registration (the peer registers
/// asynchronously during its own start-up).
const EPMD_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the peer is given to print what it received.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(15);

/// A process-unique tag, so a run's node names/cookie never collide with a
/// concurrent test's (or a leftover node from an earlier run).
fn unique(tag: &str) -> String {
    format!("beamrtc{}{}", tag, std::process::id())
}

/// The **oracle** `erl` to spawn as the peer, resolved without running it; a missing
/// toolchain is the shared loud skip ([`support::require_oracle`]).
fn oracle() -> Option<PathBuf> {
    support::require_oracle("dist_client")
}

/// The short hostname Erlang itself derives — the host part of a `-sname` node name,
/// so both nodes live in the same realm.
fn erl_hostname(oracle: &PathBuf) -> Option<String> {
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

/// A free loopback port for the peer's distribution listener.
fn free_port() -> u16 {
    for _ in 0..16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
        let port = listener.local_addr().expect("the ephemeral port").port();
        drop(listener);
        if port != 0 {
            return port;
        }
    }
    panic!("no free loopback port");
}

/// A real `erl` node acting as the **listening** peer: it registers `name` on `init`
/// and prints every message it receives on stdout, one line each.
struct PeerServer {
    child: Child,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    command: String,
}

impl PeerServer {
    /// Starts `erl -sname <node> -setcookie <cookie>` listening on `port`, with `name`
    /// registered and every received message printed as `GOT ~p`.
    fn start(
        oracle: &PathBuf,
        node: &str,
        name: &str,
        cookie: &str,
        port: u16,
    ) -> std::io::Result<PeerServer> {
        let eval = format!(
            "register({name}, self()), \
             io:format(\"READY~n\", []), \
             receive M -> io:format(\"GOT ~p~n\", [M]) end.",
            name = name
        );
        let mut child = Command::new(oracle)
            .arg("-sname")
            .arg(node)
            .arg("-setcookie")
            .arg(cookie)
            .arg("-kernel")
            .arg("inet_dist_listen_min")
            .arg(port.to_string())
            .arg("inet_dist_listen_max")
            .arg(port.to_string())
            .args(["-noshell", "-eval", &eval])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().expect("the peer's stdout is piped");
        let stderr_pipe = child.stderr.take().expect("the peer's stderr is piped");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
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

        Ok(PeerServer {
            child,
            lines,
            stderr,
            command: format!("{oracle:?} -sname {node} -setcookie {cookie} -noshell -eval {eval}"),
        })
    }

    /// Waits (bounded) for a stdout line satisfying `pred`.
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
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// The peer's stderr collected so far (for failure diagnostics).
    fn stderr_text(&self) -> String {
        self.stderr.lock().map(|buf| buf.clone()).unwrap_or_default()
    }
}

impl Drop for PeerServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a `-sname` runtime through the **CLI parser**, so its configuration is
/// exactly what a command line produces. Boots the preloaded modules and the listener.
fn node_runtime(name: &str, cookie: &str) -> Option<Runtime> {
    let args: Vec<String> = ["-noshell", "-sname", name, "-setcookie", cookie, "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config: RuntimeConfig = beam_rt::parse(&args).ok()?;
    Runtime::new(config).ok()
}

/// **The connecting half drives a real peer.** This runtime dials a real `erl` node
/// through `epmd`, completes the client side of the `'N'` handshake, and its
/// `DOP_REG_SEND` reaches a **registered process on the peer**, which prints the
/// payload — the peer's own stdout is the assertion.
#[test]
fn the_runtime_connects_to_a_real_peer_and_reg_sends_to_a_registered_name() {
    let Some(oracle) = oracle() else {
        support::skip_loudly("dist_client", "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
        return;
    };
    if epmd_unreachable() {
        eprintln!(
            "SKIP: the connector resolves its peer through `epmd`, and no daemon answers on {}",
            beam_dist::EPMD_DEFAULT_ADDR
        );
        return;
    }
    let Some(host) = erl_hostname(&oracle) else {
        eprintln!("SKIP: the system `erl` cannot report its own short hostname");
        return;
    };

    let peer_node = unique("p");
    let peer_name = unique("n"); // the registered name on the peer
    let our_node = unique("c");
    let cookie = unique("k");
    let port = free_port();

    let peer = PeerServer::start(&oracle, &peer_node, &peer_name, &cookie, port)
        .expect("the peer node starts");
    let Some(ready) = peer.wait_for(|line| line.contains("READY"), READY_TIMEOUT) else {
        panic!(
            "the real `erl` peer never printed its readiness line\n  command: {}\n  stderr: {}",
            peer.command,
            peer.stderr_text()
        );
    };
    assert!(ready.contains("READY"), "the peer's readiness line: {ready:?}");

    // The peer registers with `epmd` asynchronously during its own start-up, and the
    // connector resolves the port with a `PORT2_REQ` — so wait for the registration
    // (the pinned port is what `epmd` must hand back).
    let deadline = Instant::now() + EPMD_TIMEOUT;
    let mut resolved = None;
    while Instant::now() < deadline {
        match beam_dist::epmd::lookup_port(peer_node.as_bytes()) {
            Ok(found) => {
                resolved = Some(found);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    let Some(found) = resolved else {
        panic!(
            "the peer never appeared in `epmd` within {EPMD_TIMEOUT:?}\n  command: {}\n  stderr: {}",
            peer.command,
            peer.stderr_text()
        );
    };
    assert_eq!(found, port, "epmd resolved the peer's pinned listen port");

    let Some(mut rt) = node_runtime(&our_node, &cookie) else {
        panic!("the node runtime starts");
    };
    // One pass publishes *this* node to `epmd` too (the listening half's job).
    rt.run_one_slice().expect("a scheduler pass");

    // The connector: `epmd` lookup + TCP + the client side of the `'N'` handshake.
    rt.dist_connect(&peer_node)
        .unwrap_or_else(|err| panic!("dist_connect('{peer_node}') failed: {err:?}"));
    assert!(
        rt.dist_is_connected(&peer_node),
        "the outbound link completed its handshake"
    );
    assert_eq!(
        rt.dist_connected_nodes(),
        vec![format!("{peer_node}@{host}")],
        "the established link is reported by its full node@host name"
    );

    // A real `DOP_REG_SEND` to the peer's registered name, from this runtime's own root
    // process. The payload is a tuple, so the assertion covers messages that really
    // went through the external-term encoder.
    let mut heap = Heap::new(64);
    let hello = rt.intern("hello");
    let message = heap.tuple(&[hello, make_small(42)]).expect("the message");
    let from = rt.self_pid();
    rt.dist_send_reg_send(&peer_node, &peer_name, from, message)
        .unwrap_or_else(|err| panic!("dist_send_reg_send failed: {err:?}"));

    // The write happens on the scheduler pass's client pump (the runtime's own driving
    // of the link), so pump until the peer reports what it received.
    let deadline = Instant::now() + MESSAGE_TIMEOUT;
    let mut got = None;
    while Instant::now() < deadline {
        rt.run_one_slice().expect("a scheduler pass");
        if let Some(line) = peer.wait_for(
            |line| line.contains("GOT"),
            Duration::from_millis(50),
        ) {
            got = Some(line);
            break;
        }
    }
    let Some(got) = got else {
        panic!(
            "the peer never printed the message it should have received\n  command: {}\n  stderr: {}",
            peer.command,
            peer.stderr_text()
        );
    };
    assert!(
        got.contains("hello") && got.contains("42"),
        "the peer's registered process received the tuple this runtime sent (peer printed {got:?})"
    );
    // The link stayed up across the send (a retired link would have recorded why).
    assert!(
        rt.dist_is_connected(&peer_node),
        "the link is still established after the send"
    );
}
