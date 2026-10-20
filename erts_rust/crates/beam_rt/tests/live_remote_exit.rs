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

//! Remote **`erlang:exit/2`** (contract A.5, phase 2) proved **live against a real system
//! `erl` peer**: this runtime signals a process living on another OTP node, and the peer's
//! own node decides what to do with it.
//!
//! `erlang:exit(Target, Reason)` on an **external** pid is the C's `is_external_pid` arm —
//! `erts_dsig_send_exit2(&ctx, c_p->common.id, id, reason, prio)` → `true`
//! (`erts/emulator/beam/bif.c:1804-1838`). Here it travels the same route
//! [`tests/exit_remote.rs`](../exit_remote.rs) pins hermetically but which no real OTP node
//! had acted on before:
//! `RuntimeContext::call_bif` → the runtime-native floor
//! ([`src/native/process/exit.rs`](../../src/native/process/exit.rs) `exit_2` →
//! `PidTarget::Remote` → `ProcessTable::send_exit_term`) → `DistSignal::Exit2` →
//! `{DOP_EXIT2, From, To, Reason}` (`dist.c:1800`, *not* the link exit `DOP_EXIT`) → the
//! installed [`DistClient`](beam_rt::DistClient) router → the socket.
//!
//! # What the peer actually observes
//!
//! The peer (`-eval` below) is exit-**trapping** itself only so it can watch a *second*,
//! non-trapping process: it `spawn_monitor`s a target that prints `TARGET <hex of
//! term_to_binary(self())>` and then answers probes (`{probe, From, Tag}` →
//! `{alive, Tag}`). The watchdog reports from two windows — `DIED <reason>` if its monitor
//! fires while it waits, then, after a bounded delay, a probe whose answer it reports as
//! `ALIVE`, followed by a second window that reports `DIED <reason>` again or `NODOWN`
//! (no death at all). So the peer's own output — not a timeout on our side — is the
//! evidence:
//!
//! * [`a_remote_exit_kill_really_kills_the_peers_process`] — `exit(Target, kill)`. The
//!   target dies, the peer's monitor fires (from whichever window the death lands in) and it
//!   prints `DIED killed` (ERTS rewrites an untrappable `kill` to `killed`,
//!   `erl_proc_sig_queue.c:4768-4769`); no proof of life appears anywhere in its output.
//! * [`a_remote_exit_normal_leaves_a_non_trapping_peer_process_alive`] — the **byte-identical**
//!   session with `normal` instead of `kill`. ERTS ignores an exit signal whose reason is
//!   `normal` when the target does not trap exits (`erl_proc_sig_queue.c:4758-4763`), so the
//!   target answers the peer's **post-exit** probe (`ALIVE`) and the watch window that
//!   follows it expires with no `'DOWN'` (`NODOWN`); the transcript holds no `DIED` line and
//!   no unanswered probe, and the target demonstrably outlived the signal.
//!
//! The `kill` session is the non-vacuity control for the `normal` one: both tests share one
//! [`run_session`] and differ **only** in the reason atom, so the `normal` case's silence
//! cannot be explained by a signaller that never delivers anything — the same harness is
//! observed to kill the same peer program in the sibling test.
//!
//! The trapping-target case is deliberately **not** repeated here: it is covered
//! hermetically (and oracle-cross-checked) by `tests/exit_remote.rs`.
//!
//! # Why the link is established first
//!
//! `exit/2` on a remote pid deliberately does **not** dial: establishing a connection can
//! block, and an exit signal must never wait
//! ([`src/native/process/exit.rs:183-186`](../../src/native/process/exit.rs)), so a signal
//! with no established link to the peer's node is the typed `no_link_error` rather than a
//! silent drop. This suite therefore calls [`Runtime::dist_connect`] before the BIF —
//! exactly what an earlier `net_adm:ping/1` or any `send` to that node
//! (`Runtime::dist_ensure_link`) would have done.
//!
//! # Harness
//!
//! The peer plumbing is [`tests/live_signals.rs`](../live_signals.rs)'s /
//! [`tests/live_remote_families.rs`](../live_remote_families.rs)'s, verbatim: a pinned
//! `inet_dist_listen_min`/`_max` listener port, process-unique node names and cookie, the
//! `epmd` registration wait, and a bounded kill-and-reap on drop. The pid is published as
//! the **hex of `term_to_binary(self())`** and decoded with `beam_codec` into the
//! **runtime's own atom table**, because a `DOP_EXIT2` is addressed to an external pid
//! (node atom, number, serial **and creation**) and a wrong creation makes OTP drop the
//! signal (`erts_proc_lookup` misses); the creation is not guessable from `erl`'s printed
//! `<0.N.S>` form.
//!
//! Both tests are a **loud skip** (`SKIP: …` + early `return`, never a panic and never a
//! silent pass) when no usable `erl`/`epmd` is available.
//!
//! The runtime side is driven by [`Runtime::run_one_slice`] rather than `run_scheduled`, so
//! there is no Erlang-level `receive` to keep awake: a scheduler pass is what writes the
//! queued control to the socket (`pump_dist_client` → `DistClient::pump`) and what drains
//! the peer's answers, and the loop below drives those passes with a bounded deadline.

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
use beam_process::RemotePid;
use beam_rt::{Runtime, RuntimeConfig, RuntimeContext};

// The shared harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

/// How long the peer is given to spawn its target and publish that target's pid.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `epmd` is polled for the peer's registration (a node registers asynchronously
/// during its own start-up, and the connector resolves it with a `PORT2_REQ`).
const EPMD_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the suite keeps driving scheduler passes while the peer reports its outcome.
/// Comfortably longer than the peer's whole program ([`PEER_PROBE_DELAY_MS`] +
/// [`PROBE_REPLY_TIMEOUT_MS`] + [`LATE_DEATH_WATCH_MS`]), so a peer that never reports is a
/// loud failure with its own transcript rather than a flake.
const PEER_OUTCOME_WAIT: Duration = Duration::from_secs(40);
/// The pause between scheduler steps in a polling loop (a step is cheap, but the loop is a
/// busy wait otherwise).
const STEP_PAUSE: Duration = Duration::from_millis(10);
/// **The peer's bound on hearing about its target's death.** A delivered `DOP_EXIT2` with a
/// terminating reason kills the target within milliseconds; this bound exists only so a
/// *surviving* target's session reaches its next step — the post-exit probe. It is a
/// reporting bound, not a synchronisation: a death that lands later is still reported by the
/// watchdog's second `receive`, so the `kill` case does not depend on it.
const PEER_PROBE_DELAY_MS: u64 = 8_000;
/// How long the peer's target waits for the probe before exiting on its own (the peer is
/// killed and reaped on drop, so this only makes the session terminate by itself).
const TARGET_LIFETIME_MS: u64 = 60_000;
/// The peer's bound on the target answering its post-exit probe.
const PROBE_REPLY_TIMEOUT_MS: u64 = 5_000;
/// The watchdog's **post-exit watch window**: after the probe it waits this long for a
/// `'DOWN'` it must never see in the `normal` case, then reports `NODOWN`. That is the
/// bounded window the absence assertion rests on.
const LATE_DEATH_WATCH_MS: u64 = 3_000;

/// A process-unique tag, so a run's node names/cookie never collide with a concurrent
/// test's (or a leftover node from an earlier run).
fn unique(tag: &str) -> String {
    format!("beamrtlx{}{}", tag, std::process::id())
}

/// A session counter: libtest runs this target's tests **in parallel threads of one OS
/// process**, so a tag keyed on the process id alone would hand two concurrent sessions the
/// same `-sname`, and `erl` refuses to start ("the name … seems to be in use by another
/// Erlang node") — as does this runtime's own listener.
static SESSION: AtomicUsize = AtomicUsize::new(0);

/// The **oracle** `erl` to spawn as the peer, resolved without running it; a missing
/// toolchain is the shared loud skip ([`support::require_oracle`]).
fn oracle() -> Option<PathBuf> {
    support::require_oracle("live_remote_exit")
}

/// Whether a `beam_dist` client can reach `epmd` at all (no daemon ⇒ the peer can neither
/// register nor be resolved ⇒ loud skip).
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

/// Starts a `-sname` runtime through the **CLI parser**, so its configuration is exactly what
/// a command line produces. Boots the preloaded modules, binds the listener and (with a node
/// name) installs the [`DistSignalRouter`](beam_process::DistSignalRouter) on its process
/// table — which is what `erlang:exit/2`'s remote arm routes through.
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

/// Decodes the peer's own `term_to_binary(Pid)` image into a [`RemotePid`].
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

/// The peer's `-eval`.
///
/// `process_flag(trap_exit, true)` is the *watchdog*'s flag, never the target's: the peer
/// `spawn_monitor`s a non-trapping target so the watchdog's own `'DOWN'` (never a
/// terminating raise) is what reports the target's fate. The target publishes its own
/// `term_to_binary(self())` as hex and then answers probes.
///
/// The watchdog reports in three places, and which one appears is decided by the peer's own
/// OTP node:
///
/// * **`DIED <reason>`** — its monitor fired, either while it was waiting for the death
///   (first `receive`) or during the post-exit watch window (second `receive`). Printing it
///   from *both* windows is what makes the `kill` case independent of the timing of the
///   signal: a death that lands before [`PEER_PROBE_DELAY_MS`] is reported by the first, one
///   that lands later is reported by the second.
/// * **`ALIVE`** — after [`PEER_PROBE_DELAY_MS`] the watchdog probes the target and the
///   target answered, so the target was still there **after** the exit signal this suite
///   sent. `NOANSWER` is the same probe left unanswered.
/// * **`NODOWN`** — the watch window after the probe expired with no `'DOWN'` at all, i.e.
///   the target was still alive [`LATE_DEATH_WATCH_MS`] later and no death was reported.
///
/// Every `after` bound makes the program terminate on its own.
///
/// The target answers its probe and then **keeps living** (a second, idle `receive`) rather
/// than returning: a target that simply ended after answering would exit with `normal` and
/// trip the watchdog's own monitor, which would look exactly like a delivered `normal` exit
/// signal.
fn exit_peer_eval() -> String {
    format!(
        "process_flag(trap_exit, true), \
         {{Target, Ref}} = spawn_monitor(fun() -> \
             io:format(\"TARGET ~s~n\", \
                       [binary_to_list(binary:encode_hex(term_to_binary(self())))]), \
             receive {{probe, From, Tag}} -> From ! {{alive, Tag}} \
             after {TARGET_LIFETIME_MS} -> ok end, \
             receive after {TARGET_LIFETIME_MS} -> ok end \
         end), \
         receive \
             {{'DOWN', Ref, process, Target, Reason}} -> \
                 io:format(\"DIED ~p~n\", [Reason]) \
         after {PEER_PROBE_DELAY_MS} -> \
             Tag = make_ref(), \
             Target ! {{probe, self(), Tag}}, \
             receive {{alive, Tag}} -> io:format(\"ALIVE~n\") \
             after {PROBE_REPLY_TIMEOUT_MS} -> io:format(\"NOANSWER~n\") end, \
             receive \
                 {{'DOWN', Ref, process, Target, LateReason}} -> \
                     io:format(\"DIED ~p~n\", [LateReason]) \
             after {LATE_DEATH_WATCH_MS} -> io:format(\"NODOWN~n\") end \
         end."
    )
}

/// A real `erl` node acting as the **listening** peer: it holds its distribution listener on
/// `port` (`inet_dist_listen_min`/`_max`, the recipe `tests/live_signals.rs` uses), runs
/// `eval` as its `-eval`, and publishes its stdout/stderr line by line.
///
/// `lines` yields the peer's stdout one line at a time (`transcript` keeps all of them for
/// diagnostics **and** for asserting the absence of a line); `stderr` accumulates its
/// diagnostics. Drop kills and reaps it.
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
    fn lines_text(&self) -> Vec<String> {
        self.transcript
            .lock()
            .map(|buf| buf.clone())
            .unwrap_or_default()
    }

    /// Every stdout line the peer has printed so far, joined (failure diagnostics).
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

    /// Waits (bounded) for `node`'s `epmd` registration and returns the advertised port.
    ///
    /// A peer registers asynchronously during its own start-up, and this runtime's connector
    /// resolves the peer with a `PORT2_REQ`, so the registration must be observable before the
    /// dial is attempted ([`EPMD_TIMEOUT`] bounds it, so a peer that never registers is a loud
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

/// The reason atom the suite signals — the **only** difference between the two sessions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PeerExit {
    /// [`PeerExit::Kill`]'s reason: the target must die, and ERTS reports it as `killed`.
    Kill,
    /// [`PeerExit::Normal`]'s reason: a non-trapping target must ignore the signal entirely.
    Normal,
}

impl PeerExit {
    /// The reason atom as the oracle would spell it.
    fn reason_atom(self) -> &'static str {
        match self {
            PeerExit::Kill => "kill",
            PeerExit::Normal => "normal",
        }
    }

    /// The line the peer must end its session with for this reason.
    fn expected(self) -> &'static str {
        match self {
            PeerExit::Kill => "DIED ",
            PeerExit::Normal => "NODOWN",
        }
    }

    /// Whether `line` is one of the peer's **terminal** lines — the ones that settle what its
    /// own node did with the signal.
    ///
    /// `ALIVE` is deliberately not terminal: it may legitimately precede a late `DIED` in the
    /// `kill` session (the watchdog's probe can go out before the signal lands, and the death
    /// is then reported by its second `receive`), so waiting on it would make the `kill` case
    /// depend on timing. The `normal` session treats all three as terminal, because for it a
    /// `DIED`/`NOANSWER` is a failure the test must report rather than wait past.
    fn is_terminal(self, line: &str) -> bool {
        match self {
            PeerExit::Kill => line.starts_with("DIED "),
            PeerExit::Normal => {
                line.starts_with("NODOWN")
                    || line.starts_with("NOANSWER")
                    || line.starts_with("DIED ")
            }
        }
    }
}

/// Everything one live session observed: the peer's own **final line**, its whole stdout
/// transcript (so a test can assert an *absence*), and its command line.
struct Observation {
    line: String,
    transcript: Vec<String>,
    command: String,
}

/// Drives scheduler passes (which is what flushes and decodes distribution traffic) while
/// polling the peer's stdout for a line satisfying `pred`, bounded by `timeout`.
fn drive_until<F: Fn(&str) -> bool>(
    rt: &mut Runtime,
    peer: &Peer,
    pred: F,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        rt.run_one_slice().expect("a scheduler pass");
        if let Some(line) = peer.wait_for(&pred, Duration::from_millis(50)) {
            return Some(line);
        }
        thread::sleep(STEP_PAUSE);
    }
    None
}

/// Runs one live session — a real `erl` peer whose non-trapping target this runtime
/// `erlang:exit/2`s with `kind`'s reason — and returns what the peer observed.
///
/// Everything (the peer program, this runtime's node identity, the connector handshake, the
/// pid decoding and the BIF call) is identical for both reasons, so the only difference
/// between a `DIED killed` and an `ALIVE` is the reason carried by one `DOP_EXIT2`.
fn run_session(oracle: &Path, kind: PeerExit) -> Observation {
    let session = SESSION.fetch_add(1, Ordering::Relaxed);
    let peer_node = format!("{}{}", unique("xp"), session);
    let our_node = format!("{}{}", unique("xc"), session);
    let cookie = unique("xk");
    let port = free_port();

    let peer = Peer::start(oracle, &peer_node, &cookie, port, &exit_peer_eval())
        .expect("the real `erl` peer starts");
    let ready = peer
        .wait_for(|line| line.starts_with("TARGET "), READY_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the real `erl` peer never published its target's pid\n  command: {}\n  stdout: \
                 {}\n  stderr: {}",
                peer.command,
                peer.transcript_text(),
                peer.stderr_text()
            )
        });
    let hex = ready
        .strip_prefix("TARGET ")
        .expect("the pid line's prefix")
        .trim()
        .to_string();

    let found = peer.wait_registered(&peer_node);
    assert_eq!(found, port, "epmd resolved the peer's pinned listen port");

    let mut rt = node_runtime(&our_node, &cookie);
    // One pass publishes *this* node to `epmd` too (and finalises its creation).
    rt.run_one_slice().expect("a scheduler pass");
    // See the module docs: the BIF deliberately does not dial, so the link has to exist. This
    // is the connection an earlier `net_adm:ping/1` (or any `send` to that node) would have
    // made; without it `erlang:exit/2` answers the typed `no_link_error` instead.
    rt.dist_connect(&peer_node)
        .unwrap_or_else(|err| panic!("dist_connect('{peer_node}') failed: {err:?}"));
    assert!(
        rt.dist_is_connected(&peer_node),
        "the outbound link completed its handshake"
    );

    let target = decode_remote_pid(&mut rt, &hex);
    let reason = rt.intern(kind.reason_atom());
    // The peer's pid as the **external pid term** a program hands to `erlang:exit/2`; it must
    // live on a heap that outlives the call, so it is materialised here.
    let mut pid_heap = Heap::new(256).expect("a remote-pid heap");
    let target_term = target
        .to_eterm(&mut pid_heap)
        .expect("the peer's target pid as an external pid term");

    // **The real Erlang front door**: `erlang:exit(TargetPid, Reason)` through the ordinary
    // BIF dispatch a running program reaches. The answer must be the oracle's bare `true`
    // (`bif.c:1838`) — the transport is the only thing that can fail here, and a swallowed
    // failure would make an undelivered exit look delivered.
    let answer = {
        let mut sink = NullSink;
        let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
        ctx.call_bif("erlang", "exit", 2, &[target_term, reason])
            .unwrap_or_else(|err| panic!("erlang:exit/2 on a remote pid: {err:?}"))
    };
    assert_eq!(
        rt.atom_name_of(answer),
        Some("true"),
        "erlang:exit/2 towards a remote pid answers the atom `true`"
    );

    // Drive passes while the peer's own OTP node applies (or ignores) the signal: a pass is
    // what writes the queued control to the socket and what drains the peer's answers.
    let want = kind.expected();
    let line = drive_until(
        &mut rt,
        &peer,
        |line| kind.is_terminal(line),
        PEER_OUTCOME_WAIT,
    )
    .unwrap_or_else(|| {
        panic!(
            "the peer never reported what its own node did with the exit signal\n  command: {}\n  \
             stdout: {}\n  stderr: {}",
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

    // Neither outcome retires the link: the peer accepted the frame and answered nothing back
    // (`DOP_EXIT2` is one-way, unlike the link family's `DOP_EXIT`).
    assert!(
        rt.dist_is_connected(&peer_node),
        "the outbound link is still established after the exit"
    );

    eprintln!(
        "{}: peer observed {line:?}\n  peer command: {}",
        kind.reason_atom(),
        peer.command
    );

    Observation {
        line,
        transcript: peer.lines_text(),
        command: peer.command.clone(),
    }
}

/// **`exit(PeerPid, kill)` really kills the peer's process.**
///
/// The peer's non-trapping target is signalled with the untrappable `kill`: a real OTP node
/// terminates it and ERTS rewrites the reason to `killed` (`erl_proc_sig_queue.c:4768-4769`),
/// which is what the peer's watchdog prints from its own `'DOWN'`. The BIF's answer on this
/// side is the oracle's `true`.
#[test]
fn a_remote_exit_kill_really_kills_the_peers_process() {
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

    let observed = run_session(&oracle, PeerExit::Kill);
    assert!(
        observed.line.starts_with("DIED "),
        "the peer's own node must terminate the target it was signalled about (got {:?}, peer \
         command: {})",
        observed.line,
        observed.command
    );
    // The reason, exactly: an exit signal with reason `kill` reaches a non-trapping process as
    // `killed` (`erl_proc_sig_queue.c:4768-4769`), never as `kill`.
    let fields: Vec<&str> = observed.line.split_whitespace().skip(1).collect();
    assert_eq!(
        fields,
        vec!["killed"],
        "the peer's `'DOWN'` carries the reason ERTS rewrites `kill` to (got {:?})",
        observed.line
    );
    // Nothing else decided the session: no probe of a live target could have succeeded, and
    // the peer never reported a surviving target.
    let survivors: Vec<&String> = observed
        .transcript
        .iter()
        .filter(|line| line.starts_with("ALIVE") || line.starts_with("NODOWN"))
        .collect();
    assert!(
        survivors.is_empty(),
        "a killed target must leave no proof of life behind; peer stdout was:\n{}",
        observed.transcript.join("\n")
    );
}

/// **`exit(PeerPid, normal)` does NOT kill a non-trapping peer process.**
///
/// The byte-identical session with `normal` as the reason: ERTS ignores an exit signal whose
/// reason is `normal` when the target does not trap exits
/// (`erl_proc_sig_queue.c:4758-4763`), so the target is still there when the peer probes it
/// **after** the signal was sent, and the peer prints `ALIVE`. Neither a death nor a silent
/// target may appear anywhere in the peer's output, and this side's BIF answered `true` just
/// as in the `kill` case — so the difference is the reason, not the delivery.
///
/// The sibling [`a_remote_exit_kill_really_kills_the_peers_process`] is this test's positive
/// control: the same peer program under the same harness is *observed* to die for `kill`, so
/// `ALIVE` here cannot be explained by a signal that never reached the peer.
#[test]
fn a_remote_exit_normal_leaves_a_non_trapping_peer_process_alive() {
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

    let observed = run_session(&oracle, PeerExit::Normal);
    // `NODOWN` is the peer's **own** verdict: its post-exit probe was answered (`ALIVE`, see
    // below) and its watch window then expired with no `'DOWN'` at all, so the target
    // outlived the signal — precisely the C's `normal`-is-ignored arm.
    assert!(
        observed.line.starts_with("NODOWN"),
        "an exit signal with reason `normal` must leave a non-trapping remote process alive \
         (got {:?}, peer command: {})",
        observed.line,
        observed.command
    );
    // The proof of life, explicitly: the target answered a probe sent **after** this suite's
    // `erlang:exit/2` (the peer only probes once its first `receive`'s deadline has passed,
    // which is seconds after the BIF returned).
    assert!(
        observed
            .transcript
            .iter()
            .any(|line| line.starts_with("ALIVE")),
        "the peer's target must answer the probe it sent after the exit signal; peer stdout \
         was:\n{}",
        observed.transcript.join("\n")
    );
    // The absence, explicitly, over everything the peer printed: no `'DOWN'` fired, and the
    // probe was never left unanswered.
    let fatal: Vec<&String> = observed
        .transcript
        .iter()
        .filter(|line| line.starts_with("DIED ") || line.starts_with("NOANSWER"))
        .collect();
    assert!(
        fatal.is_empty(),
        "a non-trapping process signalled with `normal` must neither die nor stop answering; \
         peer stdout was:\n{}",
        observed.transcript.join("\n")
    );
}
