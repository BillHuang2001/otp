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

//! Two A.5 distribution families proved **live against a real system `erl` peer**:
//! the outbound **`unlink`** signal, and the inbound **`DOP_ALIAS_SEND`**.
//!
//! The sibling suites each pin one half of a link from a different angle —
//! [`tests/live_signals.rs`](../live_signals.rs) the outbound `link`/`exit` pair,
//! [`tests/live_inbound.rs`](../live_inbound.rs) an inbound `DOP_REG_SEND`,
//! [`tests/dist_client.rs`](../dist_client.rs) this runtime dialling out — and both of the
//! families here were only ever pinned *width-wise* before: `beam_dist`'s own loopback
//! driver decoded the bytes this runtime queued, and no real OTP node ever acted on them.
//! The harness (node naming, the pinned listen port, the bounded poll/kill, the
//! `epmd` registration wait, the pid-publishing peer) is [`tests/live_signals.rs`]'s,
//! verbatim.
//!
//! # Family 1 — `unlink` really unlinks (`dop_unlink_id`)
//!
//! `erlang:unlink/1` on a **remote** pid reaches
//! [`ProcessTable::unlink_term`](../../beam_process/src/ptab.rs) (through
//! `RuntimeHooksBridge::link(args, unlink = true)`), which drops the pair from the caller's
//! remote-link set **and** routes [`DistSignal::Unlink`](../../beam_process/src/remote.rs) —
//! `{DOP_UNLINK_ID, Id, From, To}` (`dist.c:1472`, `dop_unlink_id`), the OTP-26+ id form of
//! `dop_unlink`.
//!
//! The proof is a pair of sessions whose **peer program is byte-identical** and which differ
//! in exactly one step:
//!
//! * [`an_unlink_stops_the_exit_a_real_erl_peer_would_otherwise_trap`] — `link`, **`unlink`**,
//!   then route the `DOP_EXIT` that our death would emit. The peer traps exits
//!   (`process_flag(trap_exit, true)`) and must therefore report **`NOLINKEXIT`**: ERTS
//!   delivers a `DOP_EXIT` to a trapping process only when the link is actually installed
//!   (`handle_exit_signal`'s `ERTS_SIG_Q_OP_EXIT_LINKED` arm looks the sender up in the
//!   target's link tree and sets `ignore = destroy = !0` when it is absent,
//!   `erl_proc_sig_queue.c:4674-4679`), so a `DOP_UNLINK_ID` the peer accepted is what makes
//!   the exit vanish. Its transcript is additionally asserted to hold **no** `LINKEXIT` line
//!   at all, so the absence of the `{'EXIT', …}` message is recorded explicitly.
//! * [`without_the_unlink_the_exit_reaches_the_trapping_peer`] — the same session with the
//!   `unlink` step skipped, which reports `LINKEXIT <our pid> <our node> router_boom`. That is
//!   the **positive control**: it proves the peer program *does* report a trapped exit for
//!   this pair, so `NOLINKEXIT` above cannot be explained by a peer that never prints.
//!
//! Both directions of the peer's answer matter: the `DOP_LINK` must have been accepted (the
//! control prints `LINKEXIT` naming **this runtime's** pid and reason), and the
//! `DOP_UNLINK_ID` must have been accepted by a real OTP node (the subject prints nothing).
//!
//! # Family 2 — inbound `DOP_ALIAS_SEND`
//!
//! `erlang:alias/1` (the runtime-native floor, `src/native/process.rs::alias_1`) mints a
//! reference alias owned by the calling process and records it in the process table's alias
//! tree. The alias's **wire** image (the node atom and creation this runtime's own identity
//! mints, `Runtime::local_node_term`/`Runtime::node_creation`) is handed to the peer, which
//! decodes it with `binary_to_term/1` and sends to it — a `{DOP_ALIAS_SEND, From, To} -- Msg`
//! frame (`dist.c:2563-2588`, `erts_proc_sig_send_dist_altact_msg`).
//!
//! On this side the drain installs the node's
//! [`beam_dist::control::LocalIdentity`] and the decode **demotes** the wire reference to the
//! internal form (`dec_is_this_node`, `external.c:3123-3129`), so
//! [`LocalDeliverySink::deliver_to_alias`](../src/dist/inbound.rs) resolves it in the alias
//! table (`erl_proc_sig_queue.c:2318-2326`, `:6036-6049`) and enqueues the payload into the
//! owner's mailbox. The assertion is on **our** mailbox: the delivered term is read out of
//! the process table and compared structurally, element by element.
//!
//! The negative control ([`without_a_live_alias_the_alias_send_is_dropped`]) hands the peer a
//! reference this runtime **never minted an alias for** and asserts the identical session
//! leaves our mailbox empty — the C's silent drop for a reference that is not (or no longer)
//! an alias. That is what makes the positive delivery non-vacuous: it is the alias, not
//! merely the frame, that decides.
//!
//! # Harness rules
//!
//! Every test is a **loud skip** (`SKIP: …` + early `return`, never a panic and never a
//! silent pass) when no usable `erl`/`epmd` is present; every oracle child is bounded and
//! killed and reaped on drop (`tests/live_signals.rs`'s `Peer`); this suite drives
//! `Runtime::run_one_slice` in a bounded loop (a step, never a blocking pass), so nothing can
//! hang; and node names/cookie are process-unique **plus** per-session, because libtest runs
//! this target's tests in parallel threads of one OS process.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::NullSink;
use beam_engine::ExecutionContext;
use beam_process::{DistSignal, MonitorRef, RemotePid};
use beam_rt::{Runtime, RuntimeConfig, RuntimeContext};
use beam_term::prelude::{make_small, Eterm, NIL};

// The shared harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

/// How long the peer is given to publish its pid line.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `epmd` is polled for a peer's registration (a node registers asynchronously
/// during its own start-up).
const EPMD_TIMEOUT: Duration = Duration::from_secs(15);
/// The unlink peer's own `after` bound, identical for the subject and the control. It is a
/// **reporting** deadline, not a wait: the exit is routed immediately after the link (and, in
/// the subject, after the unlink), so a delivered exit is trapped within milliseconds.
const PEER_EXIT_TIMEOUT_MS: u64 = 6_000;
/// How long the suite drives scheduler passes while the peer reports the link-exit outcome.
const PEER_OUTCOME_WAIT: Duration = Duration::from_secs(25);
/// How long the suite drives passes while the peer decodes the alias and sends to it.
const ALIAS_SEND_WAIT: Duration = Duration::from_secs(25);
/// How long the suite keeps driving passes **after** the peer reported `SENT`, on the
/// negative control, to give a wrong delivery every chance to appear.
const ALIAS_ABSENCE_WAIT: Duration = Duration::from_secs(6);
/// The pause between scheduler steps in a polling loop (a step is cheap, but the loop is a
/// busy wait otherwise).
const STEP_PAUSE: Duration = Duration::from_millis(10);

/// A process-unique tag, so a run's node names/cookie never collide with a concurrent
/// test's (or a leftover node from an earlier run).
fn unique(tag: &str) -> String {
    format!("beamrtlr{}{}", tag, std::process::id())
}

/// A session counter: libtest runs this target's tests **in parallel threads of one OS
/// process**, so a tag keyed on the process id alone would hand two concurrent sessions the
/// same `-sname`, and `erl` refuses to start ("the name … seems to be in use by another
/// Erlang node") — as does this runtime's own listener.
static SESSION: AtomicUsize = AtomicUsize::new(0);

/// The **oracle** `erl` to spawn as the peer, resolved without running it; a missing
/// toolchain is the shared loud skip ([`support::require_oracle`]).
fn oracle() -> Option<PathBuf> {
    support::require_oracle("live_remote_families")
}

/// Whether a `beam_dist` client can reach `epmd` at all (no daemon ⇒ the peer can neither
/// register nor be resolved ⇒ loud skip).
fn epmd_unreachable() -> bool {
    beam_dist::epmd::connect(beam_dist::EPMD_DEFAULT_ADDR).is_err()
}

/// A free loopback port for the peer's pinned distribution listener.
fn free_port() -> u16 {
    for _ in 0..16 {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
        let port = listener.local_addr().expect("the ephemeral port").port();
        drop(listener);
        if port != 0 {
            return port;
        }
    }
    panic!("no free loopback port");
}

/// Starts a `-sname` runtime through the **CLI parser**, so its configuration is exactly what
/// a command line produces. Boots the preloaded modules and binds the listener.
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
/// decoded pid's node atom is the same `Eterm` the established link is keyed by — a phantom
/// atom would make the routed signal a typed "no established connection" error rather than a
/// silently mis-addressed one.
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

/// A real `erl` node acting as the peer: it runs `eval` as its `-eval` expression, holds its
/// distribution listener on a **pinned** port (`inet_dist_listen_min`/`_max` — the recipe
/// [`tests/live_signals.rs`](../live_signals.rs) and [`tests/dist_client.rs`](../dist_client.rs)
/// use, so `epmd` hands back a known port), and publishes its stdout/stderr line by line.
///
/// `lines` yields the peer's stdout one line at a time (`transcript` keeps all of them for
/// diagnostics); `stderr` accumulates its diagnostics. Drop kills and reaps it.
struct Peer {
    child: Child,
    lines: Receiver<String>,
    transcript: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    command: String,
}

impl Peer {
    /// Spawns `erl -sname <node> -setcookie <cookie> -kernel inet_dist_listen_min <port>
    /// inet_dist_listen_max <port> -noshell -eval <eval>`.
    fn start(
        oracle: &Path,
        node: &str,
        cookie: &str,
        port: u16,
        eval: &str,
    ) -> std::io::Result<Peer> {
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
            .args(["-noshell", "-eval", eval])
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

    /// Every stdout line the peer has printed so far (for failure diagnostics **and** for
    /// asserting the *absence* of a line).
    fn transcript_text(&self) -> String {
        self.transcript
            .lock()
            .map(|buf| buf.join("\n"))
            .unwrap_or_default()
    }

    /// The peer's stdout lines, cloned (the absence checks iterate them).
    fn lines_text(&self) -> Vec<String> {
        self.transcript
            .lock()
            .map(|buf| buf.clone())
            .unwrap_or_default()
    }

    /// The peer's stderr collected so far (for failure diagnostics).
    fn stderr_text(&self) -> String {
        self.stderr.lock().map(|buf| buf.clone()).unwrap_or_default()
    }

    /// Waits (bounded) for `node`'s `epmd` registration and returns the advertised port.
    ///
    /// A peer registers asynchronously during its own start-up, and this runtime's connector
    /// resolves the peer with a `PORT2_REQ`, so the registration must be observable before the
    /// dial is attempted (`PAUSE_TIMEOUT` bounds it, so a peer that never registers is a loud
    /// failure with its own transcript rather than a hang).
    fn wait_registered(&self, node: &str) -> u16 {
        let deadline = Instant::now() + EPMD_TIMEOUT;
        loop {
            match beam_dist::epmd::lookup_port(node.as_bytes()) {
                Ok(port) => return port,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(err) => panic!(
                    "the peer never appeared in `epmd` within {EPMD_TIMEOUT:?}: {err}\n  \
                     command: {}\n  stderr: {}",
                    self.command,
                    self.stderr_text()
                ),
            }
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------------------
// Family 1 — `unlink` (DOP_UNLINK_ID)
// ---------------------------------------------------------------------------------------

/// The link-exit peer's `-eval`: trap exits, publish `term_to_binary(self())` as hex, then
/// block for the link exit and report what the peer's own OTP node did with it.
///
/// The trap is installed *before* the pid line, so a `DOP_EXIT` that really carries a link is
/// observed as a message rather than killing the peer, and the `after` deadline makes the
/// program terminate on its own.
fn link_peer_eval() -> String {
    format!(
        "process_flag(trap_exit, true), \
         io:format(\"PEERPID ~s~n\", \
                   [binary_to_list(binary:encode_hex(term_to_binary(self())))]), \
         receive {{'EXIT', From, Reason}} -> \
         io:format(\"LINKEXIT ~p ~p ~p~n\", [From, node(From), Reason]) \
         after {PEER_EXIT_TIMEOUT_MS} -> io:format(\"NOLINKEXIT~n\") end."
    )
}

/// Runs one live `link` (+ optional `unlink`) + `exit` session and returns the peer's own
/// final line.
///
/// `unlink` selects the **subject** shape (`link` → `unlink` → `DOP_EXIT`, which must report
/// `NOLINKEXIT`) or the **positive control** (`link` → `DOP_EXIT`, which must report
/// `LINKEXIT`). Everything else — the peer program, this runtime's node identity, the
/// connector handshake and the pid decoding — is identical, so the only difference between
/// `LINKEXIT` and `NOLINKEXIT` is whether the `DOP_UNLINK_ID` was routed **and accepted**.
///
/// The `DOP_EXIT` is `DistSignal::Exit { from: our pid, to: the peer's pid }` routed through
/// the runtime-installed [`DistSignalRouter`](beam_process::DistSignalRouter) — exactly the
/// signal a dying linked process emits (`dist.c:1747` `erts_dsig_send_exit`). Routing it
/// directly (rather than killing a process) is what makes the **peer's** reaction the
/// assertion: the frame reaches the wire in both sessions.
fn run_link_session(oracle: &Path, unlink: bool) -> String {
    let session = SESSION.fetch_add(1, Ordering::Relaxed);
    let peer_node = format!("{}{}", unique("p"), session);
    let our_node = format!("{}{}", unique("c"), session);
    let cookie = unique("k");
    let port = free_port();

    let peer = Peer::start(oracle, &peer_node, &cookie, port, &link_peer_eval())
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
    let found = peer.wait_registered(&peer_node);
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

    // The router the runtime installed on its process table: `DistClient::router` hands out a
    // `DistSignalRouter` over the *same* `Rc<RefCell<DistClientInner>>`, so this is the
    // production seam, not a look-alike.
    let mut router = {
        let client = rt.dist_client().expect("a node runtime has a client");
        assert!(
            client.is_connected_to_node(remote.node()),
            "the decoded peer pid names the established link's node"
        );
        client.router()
    };

    // The peer's pid as the **external pid term** a program passes to `erlang:link/1`; it must
    // live on a heap that outlives the two calls, so it is materialised here.
    let mut pid_heap = Heap::new(256).expect("a remote-pid heap");
    let remote_term = remote
        .to_eterm(&mut pid_heap)
        .expect("the peer's pid as an external pid term");

    // **The real Erlang front door.** `erlang:link/1` reaches `RuntimeHooksBridge::link`,
    // which classifies the argument as a remote pid and hands it to
    // `ProcessTable::link_term` (record the remote link, then route `DistSignal::Link`).
    assert!(
        call_erlang_is_true(&mut rt, "link", &[remote_term]),
        "the erlang:link/1 front door answers the atom `true`"
    );
    assert_eq!(
        rt.processes().remote_links(from).len(),
        1,
        "the front door recorded exactly one remote link"
    );
    // The scheduler pass is what writes the queued control to the socket
    // (`pump_dist_client` → `DistClient::pump`).
    rt.run_one_slice().expect("a scheduler pass");

    if unlink {
        // The subject: `erlang:unlink/1` on the same pid must remove the pair **and** route
        // `{DOP_UNLINK_ID, Id, From, To}`.
        assert!(
            call_erlang_is_true(&mut rt, "unlink", &[remote_term]),
            "the erlang:unlink/1 front door answers the atom `true`"
        );
        assert!(
            rt.processes().remote_links(from).is_empty(),
            "the unlink dropped the pair from the caller's remote-link set"
        );
        rt.run_one_slice().expect("a scheduler pass");
    }

    // Now die, as a linked process's termination would: the `DOP_EXIT` for this very pair.
    router
        .route(DistSignal::Exit {
            from,
            to: remote,
            reason,
        })
        .expect("queue the outbound DOP_EXIT");

    // Drive passes while the peer's own OTP node processes what arrived (in order: the link,
    // the unlink where present, then the exit).
    let want = if unlink { "NOLINKEXIT" } else { "LINKEXIT " };
    let deadline = Instant::now() + PEER_OUTCOME_WAIT;
    let mut observed = None;
    while Instant::now() < deadline {
        rt.run_one_slice().expect("a scheduler pass");
        if let Some(line) = peer.wait_for(
            |line| line.starts_with("LINKEXIT ") || line.starts_with("NOLINKEXIT"),
            Duration::from_millis(50),
        ) {
            observed = Some(line);
            break;
        }
        thread::sleep(STEP_PAUSE);
    }
    let line = observed.unwrap_or_else(|| {
        panic!(
            "the peer never reported a link-exit outcome\n  command: {}\n  stdout: {}\n  \
             stderr: {}",
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

    // A `DOP_UNLINK_ID` the peer accepted removes the link, and a link the peer dropped would
    // have retired the connection — so the link itself must still be up either way.
    assert!(
        rt.dist_is_connected(&peer_node),
        "the outbound link is still established after the exit"
    );

    if unlink {
        // **The absence is the assertion**: no `{'EXIT', …}` for that link may have arrived,
        // so no `LINKEXIT` line may exist anywhere in the peer's transcript.
        let exits: Vec<String> = peer
            .lines_text()
            .into_iter()
            .filter(|line| line.starts_with("LINKEXIT "))
            .collect();
        assert!(
            exits.is_empty(),
            "the peer trapped an exit for a link `erlang:unlink/1` removed (peer command: {})",
            peer.command
        );
        eprintln!(
            "subject: the peer reported {line:?} and trapped NO exit for the unlinked pair\n  \
             peer command: {}",
            peer.command
        );
    } else {
        // The positive control: the identical peer program *does* see the exit, which is what
        // makes the subject's silence meaningful.
        assert!(
            line.contains(&our_node),
            "the peer attributed the exit to this runtime's pid (got {line:?})"
        );
        assert!(
            line.contains("router_boom"),
            "the peer received the exit reason this runtime routed (got {line:?})"
        );
        eprintln!(
            "control: the peer reported {line:?}\n  peer command: {}",
            peer.command
        );
    }

    line
}

/// **`unlink` really unlinks.** A real `erl` peer that traps exits sees **no**
/// `{'EXIT', our pid, _}` after `erlang:unlink/1` removed the link and the matching `DOP_EXIT`
/// was still routed to it: the peer reports `NOLINKEXIT`, and its transcript holds no
/// `LINKEXIT` line at all.
#[test]
fn an_unlink_stops_the_exit_a_real_erl_peer_would_otherwise_trap() {
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

    let line = run_link_session(&oracle, true);
    assert!(
        line.starts_with("NOLINKEXIT"),
        "after `erlang:unlink/1`, the exit `DOP_EXIT` must not reach the peer's link tree \
         (got {line:?})"
    );
}

/// **The positive control** — the byte-identical peer session with the `unlink` step skipped.
///
/// ERTS delivers a `DOP_EXIT` to a trapping process only when the link is installed
/// (`erl_proc_sig_queue.c:4674-4679`), so this session reports
/// `LINKEXIT <our pid> <our node> router_boom`. Without it, the subject's `NOLINKEXIT` could
/// not be distinguished from a peer that never prints.
#[test]
fn without_the_unlink_the_exit_reaches_the_trapping_peer() {
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

    let line = run_link_session(&oracle, false);
    assert!(
        line.starts_with("LINKEXIT "),
        "the trapping peer reports the link exit it can only see with the DOP_LINK the \
         `erlang:link/1` front door queued (got {line:?})"
    );
}

// ---------------------------------------------------------------------------------------
// Family 2 — inbound `DOP_ALIAS_SEND`
// ---------------------------------------------------------------------------------------

/// Splits an internal-reference `id` into the three `NEWER_REFERENCE_EXT` wire numbers.
///
/// This is `erts_set_ref_numbers` as [`beam_codec`](../../beam_codec/src/ref_wire.rs)
/// implements it (`REF_MASK = (1 << 18) - 1`, `REF1_NUM_MASK = 0xFFFC_0000`, and the high
/// word last; [`ref_numbers_to_id`] is its exact inverse), so a reference built here packs back
/// to the very id [`erlang:alias/1`] recorded — which is what the inbound demotion resolves in
/// the alias table.
fn wire_numbers(id: u64) -> [u32; 3] {
    [
        (id & ((1 << 18) - 1)) as u32,
        (id & 0xFFFC_0000) as u32,
        (id >> 32) as u32,
    ]
}

/// The alias-send peer's `-eval`: connect to this runtime by name, decode the reference alias
/// this runtime handed it (`binary_to_term` of the image below), send `{hello, 42}` to it, and
/// report what its own OTP node did.
///
/// The send is a **reference destination**, which ERTS cannot validate locally — it emits
/// `{DOP_ALIAS_SEND, From, To} -- Msg` and the receiver's alias table decides
/// (`dist.c:2563-2588`). That is exactly the inbound arm under test.
fn alias_peer_eval(target: &str, image: &[u8]) -> String {
    let bytes = image
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "Target = list_to_atom(\"{target}\"), \
         Ref = binary_to_term(<<{bytes}>>), \
         Ping = fun F(0) -> io:format(\"CONNFAIL~n\",[]), halt(2); \
                    F(N) -> case net_kernel:connect_node(Target) of \
                                true -> ok; \
                                _ -> timer:sleep(100), F(N - 1) \
                            end \
                end, \
         Ping(300), \
         io:format(\"CONNECTED ~p ~p~n\", [node(), Target]), \
         Ref ! {{hello, 42}}, \
         io:format(\"SENT ~p~n\", [node(Ref)]), \
         receive M -> io:format(\"BACK ~p~n\", [M]) after 20000 -> ok end."
    )
}

/// Runs one live alias-send session and returns a one-line summary of what both sides
/// observed.
///
/// `live` selects the **subject** (the peer is handed the wire image of a reference alias this
/// runtime minted and still holds, so the payload must land in the owner's mailbox) or the
/// **negative control** (the same session with a reference this runtime never minted an alias
/// for — the id differs above the low wire word, so the pack cannot collide — which the alias
/// table must silently drop, `erl_proc_sig_queue.c:6036-6046`).
///
/// The assertion is on **our** mailbox: the payload is read out of the process table entry of
/// the alias's owner and compared structurally with `{hello, 42}` (never on the peer's belief
/// about what it sent).
fn run_alias_session(oracle: &Path, live: bool) -> String {
    let session = SESSION.fetch_add(1, Ordering::Relaxed);
    let peer_node = format!("{}{}", unique("p"), session);
    let our_node = format!("{}{}", unique("c"), session);
    let cookie = unique("k");
    let port = free_port();

    let mut rt = node_runtime(&our_node, &cookie);
    // The first pass is what publishes this node to `epmd`, so a real peer can resolve it —
    // and it finalises the creation the alias's wire image must carry.
    rt.run_one_slice().expect("a scheduler pass");
    assert!(
        rt.is_alive(),
        "the node is published to epmd: {:?}",
        rt.dist_node().and_then(|node| node.registration_error())
    );
    let target = rt
        .node_name()
        .expect("a configured node has a name")
        .to_string();
    let creation = rt.node_creation();
    assert_ne!(creation, 0, "a live node mints identifiers at its own creation");

    // The reachability evidence no peer can fake: `epmd` resolves the short name to the very
    // port this runtime's listener bound.
    let deadline = Instant::now() + EPMD_TIMEOUT;
    let mut resolved = None;
    while Instant::now() < deadline {
        if let Ok(found) = beam_dist::epmd::lookup_port(our_node.as_bytes()) {
            resolved = Some(found);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let Some(resolved) = resolved else {
        panic!(
            "this node never appeared in `epmd` within {EPMD_TIMEOUT:?}: {:?}",
            rt.dist_node().and_then(|node| node.registration_error())
        );
    };
    assert_eq!(
        Some(resolved),
        rt.dist_node().map(|node| node.port()),
        "epmd resolves our short name to the port our listener bound"
    );

    // `erlang:alias/1` through the runtime-native floor: the alias is owned by the **calling**
    // process, which for this context is the runtime's own root process.
    let (owner, alias_id) = {
        let mut sink = NullSink;
        let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
        let owner = ctx.pid();
        let term = ctx
            .call_bif("erlang", "alias", 1, &[NIL])
            .expect("erlang:alias/1 mints a reference alias");
        let reference = MonitorRef::from_eterm(term)
            .expect("erlang:alias/1 answers an internal reference term");
        (owner, reference.id())
    };
    assert_eq!(
        owner,
        rt.self_pid(),
        "the alias is owned by the process that called erlang:alias/1"
    );

    // The reference the peer is handed: an **alias this runtime minted** for the subject, and
    // a reference it never minted an alias for on the control (the id differs above the low
    // wire word, so no live alias can pack to it).
    let id = if live { alias_id } else { alias_id + (1 << 40) };
    let mut ref_heap = Heap::new(64).expect("a heap for the wire reference");
    let wire = beam_codec::term::make_external_ref(
        &mut ref_heap,
        rt.local_node_term(),
        &wire_numbers(id),
        creation,
    )
    .expect("the alias as a wire reference naming this node at this creation");
    let image = beam_codec::encode_term(wire, rt.atoms())
        .expect("the wire reference encodes as an Eterm image");

    let peer = Peer::start(
        oracle,
        &peer_node,
        &cookie,
        port,
        &alias_peer_eval(&target, &image),
    )
    .expect("the real `erl` peer starts");

    // The peer's own report that it connected — a real OTP node resolving our listener through
    // `epmd` and completing the `'N'` handshake as the connecting side.
    //
    // The wait **drives the runtime**: the accept and the handshake run inside the scheduler
    // pass (`Runtime::pump_dist`, "accept a pending inbound connection and drive every live
    // link's handshake one step"), so a bare blocking wait — the oracle's `connect_node/1`
    // retrying against a listener nobody pumps — could never see `CONNECTED` at all. Every
    // step is bounded (`run_one_slice`), so this loop is a poll, never a hang.
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut connected = None;
    while Instant::now() < deadline {
        rt.run_one_slice().expect("a scheduler pass");
        if let Some(line) = peer.wait_for(
            |line| line.starts_with("CONNECTED "),
            Duration::from_millis(20),
        ) {
            connected = Some(line);
            break;
        }
        thread::sleep(STEP_PAUSE);
    }
    let connected = connected.unwrap_or_else(|| {
        panic!(
            "the peer never connected to our node\n  command: {}\n  stdout: {}\n  stderr: {}",
            peer.command,
            peer.transcript_text(),
            peer.stderr_text()
        )
    });
    assert!(
        connected.contains(&target),
        "the peer handshaked into this runtime's node `{target}` (peer printed {connected:?})"
    );

    // Drive passes until the peer says it sent (the frame is on the wire by then), and — on
    // the subject — until the payload shows up in our mailbox.
    let deadline = Instant::now() + ALIAS_SEND_WAIT;
    let mut sent = None;
    let mut delivered: Vec<Eterm> = Vec::new();
    while Instant::now() < deadline {
        rt.run_one_slice().expect("a scheduler pass");
        delivered = mailbox_payloads(&rt, owner);
        if sent.is_none() {
            sent = peer.wait_for(|line| line.starts_with("SENT "), Duration::from_millis(20));
        }
        // The subject waits for the delivery; the control only for the send, because its
        // assertion is the absence the bounded window below records.
        if (live && !delivered.is_empty()) || (!live && sent.is_some()) {
            break;
        }
        thread::sleep(STEP_PAUSE);
    }
    let sent = sent.unwrap_or_else(|| {
        panic!(
            "the peer never reported sending to the alias\n  command: {}\n  stdout: {}\n  stderr: {}",
            peer.command,
            peer.transcript_text(),
            peer.stderr_text()
        )
    });
    assert!(
        sent.contains(&target),
        "the peer sent to a reference naming this runtime's node (peer printed {sent:?})"
    );

    if live {
        assert_eq!(
            delivered.len(),
            1,
            "exactly one message reached the alias owner's mailbox (owner {owner:?}, rendered \
             {:?})",
            rendered(&mut rt, &delivered)
        );
        // The expected payload is built on a heap that **outlives the comparison**: a term is a
        // pointer into the arena that produced it, so letting the arena drop first would leave
        // `expected` dangling (and the comparison reading freed words).
        let mut expected_heap = Heap::new(64).expect("a heap for the expected payload");
        let hello = rt.atoms().lookup("hello").expect("`hello` is interned");
        let expected = expected_heap
            .tuple(&[beam_term::make_atom(hello), make_small(42)])
            .expect("the expected `{hello, 42}` payload");
        assert!(
            beam_term::compare::eq(delivered[0], expected, rt.atoms()),
            "the delivered payload is exactly `{{hello,42}}`; the runtime holds {:?}",
            rendered(&mut rt, &delivered)
        );
        let summary = format!(
            "subject: our mailbox holds {:?} (a DOP_ALIAS_SEND the peer sent to {target}); \
             peer command: {}",
            rendered(&mut rt, &delivered),
            peer.command
        );
        eprintln!("{summary}");
        return summary;
    }

    // The negative control: the same traffic, and a mailbox that must stay empty. The extra
    // window is driven with the same bounded steps (never a blocking pass), so a wrong
    // delivery would still have to appear.
    let deadline = Instant::now() + ALIAS_ABSENCE_WAIT;
    let mut delivered = mailbox_payloads(&rt, owner);
    while Instant::now() < deadline && delivered.is_empty() {
        rt.run_one_slice().expect("a scheduler pass");
        delivered = mailbox_payloads(&rt, owner);
        thread::sleep(STEP_PAUSE);
    }
    assert!(
        delivered.is_empty(),
        "a reference this runtime minted no alias for must be silently dropped \
         (erl_proc_sig_queue.c:6036-6046); the mailbox holds {:?}",
        rendered(&mut rt, &delivered)
    );
    let summary = format!(
        "control: the peer sent to a reference that is NOT a live alias and our mailbox stayed \
         empty; peer command: {}",
        peer.command
    );
    eprintln!("{summary}");
    summary
}

/// The payloads queued in `pid`'s mailbox, in FIFO order — read straight out of the process
/// table (the messages are copied into the owner's heap by the delivery, so the terms are
/// stable while it lives).
fn mailbox_payloads(rt: &Runtime, pid: beam_process::ProcessId) -> Vec<Eterm> {
    rt.processes()
        .get(pid)
        .map(|process| {
            process
                .mailbox()
                .iter()
                .map(|message| message.payload())
                .collect()
        })
        .unwrap_or_default()
}

/// Renders `terms` through the runtime's own `~w` printer (failure diagnostics only).
fn rendered(rt: &mut Runtime, terms: &[Eterm]) -> Vec<String> {
    terms
        .iter()
        .map(|term| {
            rt.format_term(*term, None)
                .unwrap_or_else(|err| format!("<unrenderable: {err}>"))
        })
        .collect()
}

/// Calls `erlang:<function>/<arity>` through the ordinary BIF dispatch and answers whether it
/// returned the atom `true` (the oracle's answer for both `link/1` and `unlink/1`).
fn call_erlang_is_true(rt: &mut Runtime, function: &str, args: &[Eterm]) -> bool {
    let answer = {
        let mut sink = NullSink;
        let mut ctx = RuntimeContext::with_output(rt, &mut sink);
        ctx.call_bif("erlang", function, args.len() as u32, args)
            .unwrap_or_else(|err| panic!("erlang:{function}/{}: {err}", args.len()))
    };
    answer == rt.intern("true")
}

/// **Inbound `DOP_ALIAS_SEND`, end to end.** A real `erl` peer decodes the wire image of a
/// reference alias this runtime minted and sends `{hello, 42}` to it; the payload is delivered
/// into the alias owner's mailbox and asserted element by element.
#[test]
fn a_real_erl_peer_sends_to_a_local_alias_and_the_message_lands_in_our_mailbox() {
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

    let summary = run_alias_session(&oracle, true);
    assert!(
        summary.starts_with("subject:"),
        "the subject session delivered through a live alias (got {summary:?})"
    );
}

/// **The negative control** — the same alias-send traffic to a reference this runtime minted
/// no alias for. ERTS drops such a message silently (`erl_proc_sig_queue.c:6036-6046`), so our
/// mailbox must stay empty; without this, the subject's delivery could not be attributed to
/// the **alias** rather than to "anything addressed to this node".
#[test]
fn without_a_live_alias_the_alias_send_is_dropped() {
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

    let summary = run_alias_session(&oracle, false);
    assert!(
        summary.starts_with("control:"),
        "the control session delivered nothing (got {summary:?})"
    );
}
