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

//! The **outbound distribution *signal* path** (contract A.5, phase 2) against a
//! **real system `erl` peer**: the `erlang:link/1` front door on a remote pid, and the
//! `beam_process::DistSignalRouter` seam a [`Runtime`](beam_rt::Runtime) installs on its
//! process table (`Runtime::new` → `ProcessTable::set_dist_router`).
//!
//! [`tests/dist_client.rs`](../dist_client.rs) pins the outbound *message* path (this
//! runtime dials a real peer and `DOP_REG_SEND`s it a payload). This suite pins the
//! outbound *signal* path, whose live end was previously unproven: the only wiring test
//! (`runtime/scheduler/tests.rs::the_outbound_distribution_client_is_wired_and_driven_by_the_runtime`)
//! is a loopback that has `beam_dist`'s own server driver decode the queued control
//! message — no real `erl` ever acted on one.
//!
//! # The proof
//!
//! One peer session establishes the link and then routes the exit for that same pair. The
//! three sessions differ **only** in how the link is established:
//!
//! 1. [`DistSignal::Link`] from this runtime's own pid to the peer's pid → `DOP_LINK`
//!    (`dist.c:2224` `{DOP_LINK, From, To}` with `From` first — the order
//!    [`beam_process::remote`](../src/../beam_process/src/remote.rs) documents and
//!    `beam_dist`'s own live interop exercises). The **subject** case establishes it by
//!    calling the real `erlang:link/1` BIF
//!    ([`RuntimeContext::call_bif`](beam_rt::RuntimeContext), the front door a program
//!    actually calls); the sibling case routes the same signal through the
//!    runtime-installed router
//!    ([`DistClient::router`](beam_rt::DistClient::router), the same `Rc` the process table
//!    holds) directly, so the seam itself keeps its own live proof;
//! 2. [`DistSignal::Exit`] for that same pair → `DOP_EXIT` (`dist.c:2760`,
//!    "`from`, which `to` is linked to, died"), routed through the router in both cases.
//!
//! The peer runs `process_flag(trap_exit, true)` and then blocks on
//! `receive {'EXIT', From, Reason} → io:format("LINKEXIT ~p ~p ~p~n", [From, node(From), Reason])`.
//! Its `LINKEXIT <our pid> beamrtlsc<N>@<host> router_boom` line can only appear if **both** control
//! messages were routed, were accepted by a real OTP node, and were acted on: ERTS
//! delivers a `DOP_EXIT` to a trapping process *only when the link is actually
//! installed* — `handle_exit_signal`'s `ERTS_SIG_Q_OP_EXIT_LINKED` arm looks the link up
//! in the target's link tree and sets `ignore = destroy = !0` when it is absent
//! (`erl_proc_sig_queue.c:4674-4679`), so the exit is silently dropped. That same
//! mechanism is the **negative control** (`without_the_link_the_exit_is_not_delivered`):
//! the byte-identical peer session with the link step skipped reports
//! `NOLINKEXIT`, which is what makes the `LINKEXIT` observation non-vacuous.
//!
//! # The peer's real pid
//!
//! A `DOP_LINK`/`DOP_EXIT` is addressed to the peer's **external pid**
//! ([`RemotePid`]) — node atom, number, serial **and creation**. The creation is not
//! guessable from `erl`'s printed `<0.N.S>` form, and a wrong creation makes OTP drop
//! the signal (`erts_proc_lookup` misses), so the peer publishes its pid as the
//! **hex of `term_to_binary(self())`** and this suite decodes it with `beam_codec` into
//! the **runtime's own atom table** — the node atom is then the very `Eterm` the
//! established link is keyed by, which
//! [`DistClient::is_connected_to_node`](beam_rt::DistClient::is_connected_to_node)
//! asserts before anything is routed.
//!
//! # The `erlang:link/1` front door, and why the router case stays
//!
//! `erlang:link/1` on a remote pid used to be a source-level capability gap here
//! (`unsupported: erlang:link/1 on a remote pid: distribution is not implemented`, exit 3).
//! `RuntimeHooksBridge::link` (`beam_rt/src/context/runtime_hooks.rs`) now classifies its
//! argument with `link_target`, so a `PidTarget::Remote` reaches
//! `ProcessTable::link_term` (`beam_process/src/ptab.rs:2781`) — which records the remote
//! link **and** routes `DistSignal::Link` through the router the runtime installed. The
//! subject case therefore drives the peer through the BIF itself, which is the route a
//! program actually takes; the router case stays beside it, because it pins the seam
//! (and its wire encoding) independently of the BIF's classification, so a regression is
//! attributed to one half or the other.
//!
//! Every test is a **loud skip** (`SKIP: …` + early `return`, never a panic and never a
//! silent pass) when no usable `erl`/`epmd` is available, the peers are killed and reaped
//! on drop, and their node names/cookie are process-unique so a run can never collide
//! with a concurrent test's or a leftover node's.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::NullSink;
use beam_engine::ExecutionContext;
use beam_process::{DistSignal, RemotePid};
use beam_rt::{Runtime, RuntimeConfig, RuntimeContext};

// The shared harness is compiled into every test target; this target only uses a
// subset of it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

/// How long the peer is given to publish its pid line.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `epmd` is polled for the peer's registration (the peer registers
/// asynchronously during its own start-up).
const EPMD_TIMEOUT: Duration = Duration::from_secs(15);
/// The subject peer's `after` bound on waiting for the link exit. The exit is routed
/// immediately after the link, so this is a safety bound, not a wait.
const SUBJECT_PEER_TIMEOUT_MS: u64 = 15_000;
/// The negative control's `after` bound — short, because the point is that the exit
/// *never* arrives.
const CONTROL_PEER_TIMEOUT_MS: u64 = 4_000;
/// How long the suite drives scheduler passes while the peer reports its outcome.
const SUBJECT_WAIT: Duration = Duration::from_secs(25);
/// The same bound for the negative control (longer than its peer-side `after`).
const CONTROL_WAIT: Duration = Duration::from_secs(12);

/// A process-unique tag, so a run's node names/cookie never collide with a concurrent
/// test's (or a leftover node from an earlier run).
fn unique(tag: &str) -> String {
    format!("beamrtls{}{}", tag, std::process::id())
}

/// A session counter, because libtest runs this target's tests **in parallel threads of one
/// OS process**: a tag keyed on the process id alone would hand two concurrent sessions the
/// same `-sname`, and `erl` refuses to start ("the name … seems to be in use by another
/// Erlang node") — as does this runtime's own listener. Every session therefore appends its
/// own ordinal.
static SESSION: AtomicUsize = AtomicUsize::new(0);

/// The **oracle** `erl` to spawn as the peer, resolved without running it; a missing
/// toolchain is the shared loud skip ([`support::require_oracle`]).
fn oracle() -> Option<PathBuf> {
    support::require_oracle("live_signals")
}

/// Whether a `beam_dist` client can reach `epmd` at all (no daemon ⇒ the peer can
/// neither register nor be resolved ⇒ loud skip).
fn epmd_unreachable() -> bool {
    beam_dist::epmd::connect(beam_dist::EPMD_DEFAULT_ADDR).is_err()
}

/// A free loopback port for the peer's pinned distribution listener.
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

/// The peer's `-eval`: trap exits, publish `term_to_binary(self())` as hex, then block
/// for the link exit and report what the peer's own OTP node did with it.
///
/// The `after` deadlines make the program **terminate** on its own, and the trap is
/// installed *before* the pid line, so a `DOP_EXIT` that really carries a link is
/// observed as a message rather than killing the peer.
fn peer_eval(exit_timeout_ms: u64) -> String {
    format!(
        "process_flag(trap_exit, true), \
         io:format(\"PEERPID ~s~n\", \
                   [binary_to_list(binary:encode_hex(term_to_binary(self())))]), \
         receive {{'EXIT', From, Reason}} -> \
         io:format(\"LINKEXIT ~p ~p ~p~n\", [From, node(From), Reason]) \
         after {exit_timeout_ms} -> io:format(\"NOLINKEXIT~n\") end."
    )
}

/// A real `erl` node acting as the **listening** peer: it holds its distribution
/// listener on `port` (`inet_dist_listen_min`/`_max`, the recipe
/// `tests/dist_client.rs` uses), publishes its pid and reports the link-exit outcome.
///
/// `lines` yields the peer's stdout one line at a time (`transcript` keeps all of them
/// for diagnostics); `stderr` accumulates its diagnostics. Drop kills and reaps it.
struct Peer {
    child: Child,
    lines: Receiver<String>,
    transcript: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    command: String,
}

impl Peer {
    /// Spawns `erl -sname <node> -setcookie <cookie> -kernel inet_dist_listen_min <port>
    /// inet_dist_listen_max <port> -noshell -eval <EXPR>`.
    fn start(
        oracle: &Path,
        node: &str,
        cookie: &str,
        port: u16,
        exit_timeout_ms: u64,
    ) -> std::io::Result<Peer> {
        let eval = peer_eval(exit_timeout_ms);
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
                "{oracle:?} -sname {node} -setcookie {cookie} -kernel inet_dist_listen_min {port} \
                 inet_dist_listen_max {port} -noshell -eval {eval}"
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

/// Starts a `-sname` runtime through the **CLI parser**, so its configuration is exactly
/// what a command line produces. Boots the preloaded modules and binds the listener.
fn node_runtime(name: &str, cookie: &str) -> Runtime {
    let args: Vec<String> = ["-noshell", "-sname", name, "-setcookie", cookie, "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config: RuntimeConfig =
        beam_rt::parse(&args).expect("the CLI parser accepts a -sname node");
    Runtime::new(config).expect("the node runtime starts")
}

/// Decodes an upper/lower-case hex string (the peer's `binary:encode_hex/1` output).
fn from_hex(hex: &str) -> Vec<u8> {
    assert!(
        hex.len() % 2 == 0,
        "an even-length hex image, got {} chars",
        hex.len()
    );
    (0..hex.len() / 2)
        .map(|i| {
            u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                .unwrap_or_else(|_| panic!("hex byte {i} of {hex:?}"))
        })
        .collect()
}

/// Decodes the peer's own `term_to_binary(self())` image into a [`RemotePid`].
///
/// The atoms are interned into the **runtime's** table (`code_server_atoms_mut`), so the
/// decoded pid's node atom is the same `Eterm` the established link is keyed by — a
/// phantom atom would make [`DistSignal::Link`] a typed "no established connection"
/// error rather than a silently mis-addressed signal.
fn decode_remote_pid(rt: &mut Runtime, hex: &str) -> RemotePid {
    let bytes = from_hex(hex);
    let mut heap = Heap::new(4096).expect("a decode heap");
    let term = {
        let atoms = rt.code_server_atoms_mut();
        beam_codec::decode_term(&bytes, &mut heap, atoms)
            .unwrap_or_else(|err| panic!("the peer's own pid image does not decode: {err:?}"))
    };
    RemotePid::from_eterm(term)
        .unwrap_or_else(|err| panic!("the decoded peer pid is not an external pid: {err:?}"))
}

/// How a session establishes the link between this runtime's own pid and the peer's.
///
/// The exit half is routed through the runtime-installed
/// [`DistSignalRouter`](beam_process::DistSignalRouter) in every non-`None` case: this
/// suite proves the **link** front door, not an `exit/2` one (and a dying process's own
/// termination drains its `remote_links` into `DistSignal::Exit` anyway).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LinkRoute {
    /// The **real front door**: `erlang:link/1` on the peer's external pid term through the
    /// ordinary BIF dispatch a running program's `link(Pid)` reaches.
    Bif,
    /// The [`DistClient`](beam_rt::DistClient)-installed
    /// [`DistSignalRouter`](beam_process::DistSignalRouter) directly — the seam the BIF route
    /// delegates to, pinned independently of the BIF's own classification.
    Router,
    /// No link at all: the **negative control**.
    None,
}

/// Runs one live session and returns the peer's own final line.
///
/// `route` selects the **subject** shape (establish the link — through `erlang:link/1` or
/// through the router seam — then route `DOP_EXIT`) or the **negative control** (route only
/// the `DOP_EXIT`). Everything else — the peer, this runtime's node identity, the connector
/// handshake and the pid decoding — is identical, so the only difference between a
/// `LINKEXIT` and a `NOLINKEXIT` is whether the `DOP_LINK` was routed.
fn run_session(oracle: &Path, route: LinkRoute) -> String {
    let link = route != LinkRoute::None;
    let session = SESSION.fetch_add(1, Ordering::Relaxed);
    let peer_node = format!("{}{}", unique("p"), session);
    let our_node = format!("{}{}", unique("c"), session);
    let cookie = unique("k");
    let port = free_port();
    let peer_timeout_ms = if link {
        SUBJECT_PEER_TIMEOUT_MS
    } else {
        CONTROL_PEER_TIMEOUT_MS
    };

    let peer = Peer::start(oracle, &peer_node, &cookie, port, peer_timeout_ms)
        .expect("the real `erl` peer starts");
    let ready = peer
        .wait_for(|line| line.starts_with("PEERPID "), READY_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the real `erl` peer never published its pid\n  command: {}\n  stdout: {}\n  \
                 stderr: {}",
                peer.command,
                peer.transcript_text(),
                peer.stderr_text()
            )
        });
    let hex = ready
        .strip_prefix("PEERPID ")
        .expect("the pid line's prefix")
        .trim()
        .to_string();

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

    let mut rt = node_runtime(&our_node, &cookie);
    // One pass publishes *this* node to `epmd` too (and finalises its creation).
    rt.run_one_slice().expect("a scheduler pass");
    rt.dist_connect(&peer_node)
        .unwrap_or_else(|err| panic!("dist_connect('{peer_node}') failed: {err:?}"));
    assert!(
        rt.dist_is_connected(&peer_node),
        "the outbound link completed its handshake"
    );

    let remote = decode_remote_pid(&mut rt, &hex);
    let from = rt.self_pid();
    let reason = rt.intern("router_boom");

    // The router the runtime installed on its process table: `DistClient::router` hands
    // out a `DistSignalRouter` over the *same* `Rc<RefCell<DistClientInner>>`, so this is
    // the production seam, not a look-alike.
    let mut router = {
        let client = rt.dist_client().expect("a node runtime has a client");
        assert!(
            client.is_connected_to_node(remote.node()),
            "the decoded peer pid names the established link's node"
        );
        client.router()
    };

    if link {
        match route {
            LinkRoute::Bif => {
                // **The real Erlang front door.** `erlang:link/1` reaches
                // `RuntimeHooksBridge::link`, which classifies the argument as a remote pid and
                // hands it to `ProcessTable::link_term` (record the remote link, then route
                // `DistSignal::Link` through the installed router). The external pid term must
                // live on a heap that outlives the call, so it is materialised here.
                let mut pid_heap = Heap::new(256).expect("a remote-pid heap");
                let remote_term = remote
                    .to_eterm(&mut pid_heap)
                    .expect("the peer's pid as an external pid term");
                let answer = {
                    let mut sink = NullSink;
                    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
                    ctx.call_bif("erlang", "link", 1, &[remote_term])
                        .expect("erlang:link/1 on a remote pid")
                };
                assert_eq!(
                    rt.atom_name_of(answer),
                    Some("true"),
                    "the erlang:link/1 front door answers the atom `true`"
                );
                assert_eq!(
                    rt.processes().remote_links(from).len(),
                    1,
                    "the front door recorded exactly one remote link"
                );
            }
            LinkRoute::Router => {
                router
                    .route(DistSignal::Link {
                        from,
                        to: remote,
                    })
                    .expect("queue the outbound DOP_LINK");
            }
            LinkRoute::None => {}
        }
        // The scheduler pass is what writes the queued control to the socket
        // (`pump_dist_client` → `DistClient::pump`).
        rt.run_one_slice().expect("a scheduler pass");
    }
    router
        .route(DistSignal::Exit {
            from,
            to: remote,
            reason,
        })
        .expect("queue the outbound DOP_EXIT");

    // Drive passes while the peer's own OTP node processes what arrived.
    let (want, wait) = if link {
        ("LINKEXIT ", SUBJECT_WAIT)
    } else {
        ("NOLINKEXIT", CONTROL_WAIT)
    };
    let deadline = Instant::now() + wait;
    let mut observed = None;
    while Instant::now() < deadline {
        rt.run_one_slice().expect("a scheduler pass");
        let found = peer.wait_for(
            |line| line.starts_with("LINKEXIT ") || line.starts_with("NOLINKEXIT"),
            Duration::from_millis(50),
        );
        if let Some(line) = found {
            observed = Some(line);
            break;
        }
    }
    let line = observed.unwrap_or_else(|| {
        panic!(
            "the peer never reported a link-exit outcome\n  command: {}\n  stdout: {}\n  stderr: {}",
            peer.command,
            peer.transcript_text(),
            peer.stderr_text()
        )
    });
    assert!(
        line.starts_with(want),
        "expected the peer's `{want}` line, got {line:?} (peer command: {})",
        peer.command
    );

    eprintln!(
        "{} peer reported: {line}\n  peer command: {}",
        if link { "subject:" } else { "control:" },
        peer.command
    );

    if link {
        // A `DOP_LINK` the peer rejected would have been answered with a `noproc` exit,
        // and a link the peer dropped would have retired the connection.
        assert!(
            rt.dist_is_connected(&peer_node),
            "the outbound link is still established after the exit"
        );
        // The exit the peer saw is attributed to *this* runtime's pid (`~p` renders a
        // non-local pid as `<node@host.number.serial>`).
        assert!(
            line.contains(&our_node),
            "the peer attributed the exit to this runtime's pid (got {line:?})"
        );
        assert!(
            line.contains("router_boom"),
            "the peer received the exit reason this runtime routed (got {line:?})"
        );
    }

    line
}

/// **The `erlang:link/1` front door, end to end.** A real program's `link(Pid)` on the
/// peer's remote pid establishes a `DOP_LINK` through the production BIF dispatch (not
/// through the router seam directly), and a `DOP_EXIT` for that same pair is then acted on
/// by a real `erl` node: the linked, exit-trapping peer process receives
/// `{'EXIT', OurPid, router_boom}` and prints it.
#[test]
fn a_bif_link_and_exit_reach_a_real_erl_peer() {
    let Some(oracle) = oracle() else {
        return; // `require_oracle` already reported the loud skip
    };
    if epmd_unreachable() {
        eprintln!(
            "SKIP: the peer must register with `epmd` (and this runtime resolves it there), \
             and no daemon answers on {}",
            beam_dist::EPMD_DEFAULT_ADDR
        );
        return;
    }

    let line = run_session(&oracle, LinkRoute::Bif);
    assert!(
        line.starts_with("LINKEXIT "),
        "the real peer reported the link exit it could only see with the DOP_LINK the \
         `erlang:link/1` front door queued (got {line:?})"
    );
}

/// **The router seam, end to end.** The same proof with the `DistSignal::Link` routed
/// through the runtime-installed [`beam_process::DistSignalRouter`] directly, so a
/// regression is attributable to the seam itself rather than to the BIF's own remote-pid
/// classification.
#[test]
fn a_router_routed_link_and_exit_reach_a_real_erl_peer() {
    let Some(oracle) = oracle() else {
        return; // `require_oracle` already reported the loud skip
    };
    if epmd_unreachable() {
        eprintln!(
            "SKIP: the peer must register with `epmd` (and this runtime resolves it there), \
             and no daemon answers on {}",
            beam_dist::EPMD_DEFAULT_ADDR
        );
        return;
    }

    let line = run_session(&oracle, LinkRoute::Router);
    assert!(
        line.starts_with("LINKEXIT "),
        "the real peer reported the link exit it could only see with the DOP_LINK routed \
         through the runtime's own `DistSignalRouter` (got {line:?})"
    );
}

/// **The negative control** — the same session with the link step skipped.
///
/// ERTS drops a `DOP_EXIT` whose `from` is not in the target's link tree
/// (`erl_proc_sig_queue.c:4674-4679`), so a byte-identical, exit-trapping peer that was
/// never linked prints `NOLINKEXIT`. Without this, the `LINKEXIT` above could not be
/// distinguished from "the peer prints on any `DOP_EXIT`".
#[test]
fn without_the_link_the_exit_is_not_delivered() {
    let Some(oracle) = oracle() else {
        return; // `require_oracle` already reported the loud skip
    };
    if epmd_unreachable() {
        eprintln!(
            "SKIP: the peer must register with `epmd` (and this runtime resolves it there), \
             and no daemon answers on {}",
            beam_dist::EPMD_DEFAULT_ADDR
        );
        return;
    }

    let line = run_session(&oracle, LinkRoute::None);
    assert!(
        line.starts_with("NOLINKEXIT"),
        "with no DOP_LINK routed, the peer's exit-trapping process must not receive an exit \
         (got {line:?})"
    );
}
