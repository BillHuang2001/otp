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

//! The distribution **connector BIFs** (contract A.5) against a **real system `erl` peer**:
//! `net_adm:ping/1`, `net_kernel:connect_node/1`, `erlang:nodes/0`, and the `{Name, Node}`
//! form of `erlang:send/2`.
//!
//! The runtime-native floor owns these keys (`src/native/net.rs`): they read or drive this
//! runtime's own link table, which no `beam_bifs` module can reach. This suite pins them end
//! to end against a real `erl -sname` peer started on a **pinned** distribution port, the
//! same harness [`tests/dist_client.rs`](../dist_client.rs) uses for the raw
//! `Runtime::dist_connect`/`Runtime::dist_send_reg_send` API — here the assertions go
//! through the *BIF* surface instead.
//!
//! The peer's own stdout is the assertion for the send: it prints every message it receives,
//! so the test never depends on this runtime's belief about what it sent.
//!
//! Every test here is a **loud skip** (`SKIP: …` + early `return`, never a panic and never a
//! silent pass) when no usable `erl`/`epmd` is available, and the peer is killed and reaped
//! on drop.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use beam_engine::ExecutionContext;
use beam_rt::{Runtime, RuntimeConfig, RuntimeContext};
use beam_term::Eterm;

// The shared harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

/// How long the peer is given to print its readiness line.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `epmd` is polled for the peer's registration (the peer registers
/// asynchronously during its own start-up).
const EPMD_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the peer is given to print what it received.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(15);

/// A process-unique tag, so a run's node names/cookie never collide with a concurrent
/// test's (or a leftover node from an earlier run).
fn unique(tag: &str) -> String {
    format!("beamrtn{}{}", tag, std::process::id())
}

/// The **oracle** `erl` to spawn as the peer, resolved without running it; a missing
/// toolchain is the shared loud skip ([`support::require_oracle`]).
fn oracle() -> Option<PathBuf> {
    support::require_oracle("net_adm")
}

/// The short hostname Erlang itself derives — the host part of a `-sname` node name, so both
/// nodes live in the same realm.
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

/// Whether a `beam_dist` client can reach `epmd` at all (no daemon ⇒ the peer can neither
/// register nor be resolved ⇒ loud skip).
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

/// A real `erl` node acting as the peer: it registers `name` on `init` and prints every
/// message it receives on stdout, one line each.
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

/// Starts a `-sname` runtime through the **CLI parser**, so its configuration is exactly
/// what a command line produces. Boots the preloaded modules and the listener.
fn node_runtime(name: &str, cookie: &str) -> Option<Runtime> {
    let args: Vec<String> = ["-noshell", "-sname", name, "-setcookie", cookie, "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config: RuntimeConfig = beam_rt::parse(&args).ok()?;
    Runtime::new(config).ok()
}

/// Calls a BIF through the runtime's `ExecutionContext` seam, printing nothing.
fn call_bif(
    rt: &mut Runtime,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> beam_common::Result<Eterm> {
    let mut sink = beam_bifs::NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    ctx.call_bif(module, function, args.len() as u32, args)
}

/// A tuple built on the runtime's heap (through the engine seam, so it stays valid).
fn tuple(rt: &mut Runtime, elements: &[Eterm]) -> Eterm {
    let mut sink = beam_bifs::NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    beam_engine::terms::make_tuple(&mut ctx, elements).expect("a tuple")
}

/// A list built on the runtime's heap (through the engine seam, so it stays valid).
fn list(rt: &mut Runtime, elements: &[Eterm]) -> Eterm {
    let mut sink = beam_bifs::NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    beam_engine::terms::make_list(&mut ctx, elements).expect("a list")
}

/// `term` rendered by the registered `~p` printer (with the trailing newline stripped).
fn rendered(rt: &mut Runtime, term: Eterm) -> String {
    rt.format_term(term, None)
        .expect("a printable term")
        .trim_end()
        .to_string()
}

/// **The connector BIFs drive a real peer.** Against one live `erl -sname` node this pins:
/// `net_adm:ping/1` ⇒ `pong` (and the link it established), `erlang:nodes/0` ⇒ that peer,
/// `net_kernel:connect_node/1` ⇒ `true` for the peer / `false` for an unreachable node, and
/// the `{Name, Node}` form of `erlang:send/2` really reaching the peer's registered process.
#[test]
fn the_connector_bifs_reach_a_real_peer() {
    let Some(oracle) = oracle() else {
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
    let peer_full = format!("{peer_node}@{host}");

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

    // The peer registers with `epmd` asynchronously during its own start-up; the connector
    // resolves the port with a `PORT2_REQ` (under the node's **short** name, which is what
    // this OTP release registers), so wait for the registration.
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
    // One pass publishes *this* node to `epmd` too: `net_kernel:connect_node/1` answers
    // `ignored` on a runtime that is not **alive**, so the connector test needs the
    // registration to have completed.
    rt.run_one_slice().expect("a scheduler pass");
    if !rt.is_alive() {
        eprintln!(
            "SKIP: `epmd` did not accept this runtime's ALIVE2 registration, so \
             net_kernel:connect_node/1 can only answer `ignored`"
        );
        return;
    }

    // `net_adm:ping/1` -- the full `node@host` form, which the real connector resolves
    // through the peer's short name. This also establishes the link.
    let peer_atom = rt.intern(&peer_full);
    let ping = call_bif(&mut rt, "net_adm", "ping", &[peer_atom]).expect("net_adm:ping/1");
    assert_eq!(rendered(&mut rt, ping), "pong", "net_adm:ping('{peer_full}')");
    assert!(
        rt.dist_is_connected(&peer_node),
        "net_adm:ping established the outbound link"
    );
    assert_eq!(
        rt.dist_connected_nodes(),
        vec![peer_full.clone()],
        "the established link is reported by its full node@host name"
    );

    // `erlang:nodes/0` -- the runtime's own link table, not a registry value.
    let nodes = call_bif(&mut rt, "erlang", "nodes", &[]).expect("erlang:nodes/0");
    assert_eq!(rendered(&mut rt, nodes), format!("[{peer_full}]"));
    // `nodes(connected)`/`visible` select peers, `hidden` has none, `this` is the local node.
    let connected_atom = rt.intern("connected");
    let connected = call_bif(&mut rt, "erlang", "nodes", &[connected_atom]).expect("nodes(connected)");
    assert_eq!(rendered(&mut rt, connected), format!("[{peer_full}]"));
    let hidden_atom = rt.intern("hidden");
    let hidden = call_bif(&mut rt, "erlang", "nodes", &[hidden_atom]).expect("nodes(hidden)");
    assert_eq!(rendered(&mut rt, hidden), "[]", "no hidden links are modelled");
    let this_atom = rt.intern("this");
    let this = call_bif(&mut rt, "erlang", "nodes", &[this_atom]).expect("nodes(this)");
    assert_eq!(rendered(&mut rt, this), format!("[{our_node}@{host}]"));
    // The oracle's ordering for a mixed selector: peers first, the local node last.
    let mixed = list(&mut rt, &[this_atom, connected_atom]);
    let both = call_bif(&mut rt, "erlang", "nodes", &[mixed]).expect("nodes([this,connected])");
    assert_eq!(
        rendered(&mut rt, both),
        format!("[{peer_full},{our_node}@{host}]"),
        "peers first, the local node last"
    );

    // `net_kernel:connect_node/1`: `true` for the (already connected) peer, `false` for a
    // node neither `epmd` nor the network can reach.
    let connected_ok = call_bif(&mut rt, "net_kernel", "connect_node", &[peer_atom])
        .expect("net_kernel:connect_node/1");
    assert_eq!(rendered(&mut rt, connected_ok), "true", "connect_node(peer)");

    let unreachable = rt.intern("nosuchnode@nowhere");
    let failed = call_bif(&mut rt, "net_kernel", "connect_node", &[unreachable])
        .expect("net_kernel:connect_node/1");
    assert_eq!(
        rendered(&mut rt, failed),
        "false",
        "an unreachable node is `false`, not `ignored` (this runtime is alive)"
    );

    // `net_adm:ping/1` on the unreachable node: the `pang` half of the same answer.
    let pang = call_bif(&mut rt, "net_adm", "ping", &[unreachable]).expect("net_adm:ping/1");
    assert_eq!(rendered(&mut rt, pang), "pang");

    // The remote send through the `erlang:send/2` BIF: a `{Name, Node}` tuple destination,
    // transmitted as a `DOP_REG_SEND` to the peer's registered process. The peer's own stdout
    // is the assertion.
    let name_atom = rt.intern(&peer_name);
    let destination = tuple(&mut rt, &[name_atom, peer_atom]);
    let hello = rt.intern("hello");
    let message = tuple(&mut rt, &[hello, beam_term::make_small(42)]);
    let sent = call_bif(&mut rt, "erlang", "send", &[destination, message]).expect("erlang:send/2");
    assert_eq!(
        sent, message,
        "the oracle's `send/2` answers the message it accepted"
    );

    // The write happens on the scheduler pass's client pump (the runtime's own driving of the
    // link), so pump until the peer reports what it received.
    let deadline = Instant::now() + MESSAGE_TIMEOUT;
    let mut got = None;
    while Instant::now() < deadline {
        rt.run_one_slice().expect("a scheduler pass");
        if let Some(line) = peer.wait_for(|line| line.contains("GOT"), Duration::from_millis(50)) {
            got = Some(line);
            break;
        }
    }
    let Some(got) = got else {
        panic!(
            "the peer never printed the message `erlang:send/2` should have delivered\n  \
             command: {}\n  stderr: {}",
            peer.command,
            peer.stderr_text()
        );
    };
    assert!(
        got.contains("hello") && got.contains("42"),
        "the peer's registered process received the tuple this runtime sent (peer printed {got:?})"
    );
    assert!(
        rt.dist_is_connected(&peer_node),
        "the link is still established after the send"
    );
}
