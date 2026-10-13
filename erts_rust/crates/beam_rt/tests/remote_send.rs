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

//! The **send-destination classifier** on a runtime that is not a distributed node, and
//! the **remote `Dest ! Msg` route** of a runtime that is one.
//!
//! `src/context/send.rs::classify_send_destination` is the one classification the three
//! send routes share — the compiled `Dest ! Msg` instruction, the `-eval` evaluator's `!`
//! operator and the `erlang:send/2`/`'!'/2` BIF (`SendBridge`) — and this suite pins what
//! it decides for the two destinations that are *not* a local pid:
//!
//! 1. **a `{Name, Node}` pair naming another node** stays the typed capability gap on a
//!    runtime with no `-name`/`-sname` — never a silent drop and never a fabricated value
//!    (the corpus assumption: the `unsupported: …` signal, exit 3);
//! 2. **a `{Name, Node}` pair naming *this* node** is a **local named send** — the C's
//!    `dep == erts_this_dist_entry` arm (`erts/emulator/beam/bif.c:2628-2655`), which runs
//!    the very same `erts_whereis_name_to_id` the atom-destination arm uses and, when the
//!    lookup finds nothing, reports success *without* delivering (`bif.c:2654`'s
//!    `return 0`) rather than `SEND_BADARG`. No distribution is involved.
//!
//! # Oracle ground truth (system `erl`, OTP 29.1.1 / erts-17.1)
//!
//! Measured with `erl -noshell -eval … -s init stop` (each probe ends in `halt(0).`):
//!
//! ```none
//! 1> node().                                                          %% nonode@nohost
//! 2> register(foo, self()), {foo, node()} ! hi,
//!    receive M -> io:format("got ~p~n",[M]) after 1000 -> io:format("timeout~n") end.
//! got hi                                  %% a LOCAL named send — no distribution
//! 3> {noreg, node()} ! hi.                %% hi — accepted, dropped (bif.c:2654)
//! 4> erlang:send({noreg, node()}, hi).    %% hi
//! 5> catch ({foo, bar} ! hi).             %% hi — the tuple is accepted whatever the
//!                                         %% node atom is; the message is dropped
//! 6> catch ({foo, 'other@nohost'} ! hi).  %% hi — same, nothing delivered
//! 7> catch ({foo, 42} ! hi).              %% {'EXIT',{badarg,…}}
//! 8> catch (noregistered ! hi).           %% {'EXIT',{badarg,…}}
//! ```
//!
//! Probes 2–4 hold on a non-distributed node **and** on a `-sname` node (the same-node arm
//! never touches the link table), which is why case (2) above is asserted on a plain
//! [`Runtime::with_defaults`] here — no peer is needed to pin it.
//!
//! # The intentional divergence (unchanged, and preserved by this suite)
//!
//! The oracle *accepts* probe 6 (`{foo, 'other@nohost'} ! hi` ⇒ `hi`) and silently drops
//! the message, because a node it cannot reach is not an error for `remote_send`. This
//! runtime has no way to reach `other@nohost` (or, on a plain runtime, **any** other node)
//! and therefore reports the typed capability gap `unsupported: erlang:send/2 to
//! foo@other@nohost: distributed send is not implemented` (exit 3) — the crate's
//! established signal for a Milestone-1 gap, which the differential corpus pins. A remote
//! **pid** destination takes the other documented route: `Runtime::dist_send_message_pid`
//! answers `distribution: this runtime is not a distributed node (no -name/-sname)` (exit
//! 1, an ordinary runtime error rather than the `unsupported` gap). Both are refusals,
//! never a silent drop; the asymmetry is reported in this suite's docs rather than
//! papered over.
//!
//! # The live half
//!
//! [`a_compiled_bang_delivers_a_remote_name_destination_to_a_real_peer`] starts a real
//! `erl -sname` peer that registers a name and prints every message it receives, and drives
//! the **compiled** `{Name, PeerNode} ! Msg` route of *this* runtime (a `-sname` node with
//! an established outbound link) at it. The peer's own stdout is the assertion, so nothing
//! here depends on this runtime's belief about what it sent.
//! [`without_a_node_the_same_tuple_is_refused_and_nothing_is_delivered`] is the **negative
//! control**: the byte-identical peer, the byte-identical compiled `!`, but a runtime with
//! no `-sname` — whose classifier refuses the tuple before anything is transmitted — so the
//! peer stays silent. Without it, "the peer printed GOT" could not be distinguished from
//! "the peer prints on anything".
//!
//! Every test is a **loud skip** (`SKIP: …` + early `return`, never a panic and never a
//! silent pass) when no usable `erl`/`epmd` is available, the peers are killed and reaped
//! on drop, their node names/cookie are process-unique, and every `erl` probe carries
//! `-s init stop` (`tests/support/mod.rs`).

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use beam_rt::Runtime;
use beam_term::prelude::*;

// The shared harness is compiled into every test target; this target only uses a
// subset of it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

/// The committed fixture (`tests/fixtures/remote_send.erl`): the compiled `!` routes and
/// the `erlang:send/2` BIF route over one shared destination grammar.
const FIXTURE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/remote_send.beam"
);

/// The node name the runtime's own `node()` answers on a plain runtime (no
/// `-name`/`-sname`), i.e. the atom a *same-node* `{Name, Node}` pair names there.
const NONODE: &str = "nonode@nohost";

/// How long the peer is given to print its readiness line.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `epmd` is polled for the peer's registration.
const EPMD_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the peer is given to print what it received.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the negative control's peer is given to prove it stayed silent.
const SILENCE_WINDOW: Duration = Duration::from_secs(3);

/// A process-unique tag, so a run's node names/cookie never collide with a concurrent
/// test's (or a leftover node from an earlier run).
fn unique(tag: &str) -> String {
    format!("beamrtrs{}{}", tag, std::process::id())
}

/// The **oracle** `erl`, resolved without running it; a missing toolchain is the shared
/// loud skip ([`support::require_oracle`]).
fn oracle() -> Option<PathBuf> {
    support::require_oracle("remote_send")
}

/// A runtime with the committed fixture loaded, so `spawn_process` resolves
/// `remote_send` from the code server's cache exactly as a `-pa` directory would.
fn runtime_with_fixture() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(FIXTURE_BEAM).expect("the fixture loads");
    rt
}

/// A fixture function run to completion as a **scheduled process**.
///
/// `Ok(value)` when the process returned normally (its result term, if any);
/// `Err(reason)` when it raised, with `reason` the reason atom's name (or a `Debug`
/// rendering when the reason is not a bare atom).
fn run_fixture(
    rt: &mut Runtime,
    function: &str,
    args: Vec<beam_term::Eterm>,
) -> Result<Option<beam_term::Eterm>, String> {
    let pid = rt
        .spawn_process("remote_send", function, args)
        .unwrap_or_else(|error| panic!("spawn remote_send:{function} failed: {error:?}"));
    rt.run_scheduler().expect("the pass drains the queue");
    match rt.process_exit(pid) {
        Some(reason) => Err(rt
            .atom_name_of(reason)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{reason:?}"))),
        None => Ok(rt.process_result(pid)),
    }
}

/// Runs the built `beam_rt` binary with `args` and returns its exit code, stdout and
/// stderr.
fn beam_rt_cli(args: &[&str]) -> (Option<i32>, String, String) {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"));
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Runs `expr` through the executable's `-eval` and returns `(exit code, stdout, stderr)`.
fn eval_cli(expr: &str) -> (Option<i32>, String, String) {
    beam_rt_cli(&["-noshell", "-eval", expr])
}

/// Whether a `beam_dist` client can reach `epmd` at all (no daemon ⇒ a peer can neither
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

/// The short hostname Erlang itself derives — the host part of a `-sname` node name, so
/// both nodes live in the same realm.
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

/// A real `erl` node acting as the **listening** peer: it registers `name` on `init` and
/// prints every message it receives on stdout, one line each.
///
/// It blocks in `receive` (a `-sname` node whose `-eval` returns would keep running with
/// an idle shell loop), so [`Peer::drop`] is what ends it.
struct Peer {
    child: Child,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    command: String,
}

impl Peer {
    /// Spawns `erl -sname <node> -setcookie <cookie>` with its distribution listener
    /// pinned to `port`, registers `name`, prints `READY`, and then reports every message
    /// it receives as `GOT ~p`.
    fn start(
        oracle: &Path,
        node: &str,
        name: &str,
        cookie: &str,
        port: u16,
    ) -> std::io::Result<Peer> {
        let eval = format!(
            "register({name}, self()), \
             io:format(\"READY~n\", []), \
             receive M -> io:format(\"GOT ~p~n\", [M]) after 60000 -> io:format(\"IDLE~n\") end.",
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

        Ok(Peer {
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

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a `-sname` runtime through the **CLI parser**, so its configuration is exactly
/// what a command line produces. One pass publishes it to `epmd`.
fn node_runtime(name: &str, cookie: &str) -> Option<Runtime> {
    let args: Vec<String> = ["-noshell", "-sname", name, "-setcookie", cookie, "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config = beam_rt::parse(&args).ok()?;
    Runtime::new(config).ok()
}

// ---------------------------------------------------------------------------------------
// 1. A remote destination on a NON-distributed runtime is refused, not dropped
// ---------------------------------------------------------------------------------------

/// **A `{Name, OtherNode}` pair is the typed capability gap on a plain runtime.**
///
/// The differential corpus pins exactly this signal, so the assertion is on the stable
/// external contract: exit code 3 and the single stderr line `beam_rt: unsupported: …`.
///
/// Oracle (probe 6 in the module docs): `hi` — accepted and silently dropped, because a
/// node it cannot reach is not an error for `remote_send`. This runtime cannot reach it at
/// all, so it reports the gap instead of fabricating the oracle's value.
#[test]
fn a_remote_name_destination_is_the_typed_capability_gap_on_a_plain_runtime() {
    let (code, stdout, stderr) = eval_cli("{foo, 'other@nohost'} ! hi.");
    assert_eq!(
        code,
        Some(3),
        "a {{Name, OtherNode}} destination is a capability gap (exit 3)\n--- stderr ---\n{stderr}"
    );
    assert!(
        stderr.starts_with("beam_rt: unsupported: "),
        "the unsupported signal is a single stderr line, got {stderr:?}"
    );
    assert!(
        stderr.contains("distributed send is not implemented"),
        "the reason names the missing capability, got {stderr:?}"
    );
    assert_eq!(stdout, "", "an uncaught raise prints nothing on stdout");
}

/// **A remote *pid* destination is refused too, by the other documented route.**
///
/// `Runtime::dist_send_message_pid` answers `distribution: this runtime is not a
/// distributed node (no -name/-sname)` — an ordinary runtime error (exit 1), *not* the
/// `unsupported` gap the tuple route uses. The important half is that it is a **refusal**:
/// a destination this runtime cannot serve never becomes a silent drop.
///
/// The pid is decoded from a hand-built `NEW_PID_EXT` image for `other@nohost` (the same
/// shape `binary_to_term/1` accepts), so the case needs no peer.
#[test]
fn a_remote_pid_destination_is_refused_on_a_plain_runtime() {
    // 131, 88 (NEW_PID_EXT), 119 (SMALL_ATOM_UTF8_EXT, len 12), "other@nohost",
    // id, serial, creation.
    let expr = concat!(
        "binary_to_term(<<131,88,119,12,\"other@nohost\",",
        "0,0,0,1,0,0,0,0,0,0,0,0>>) ! hi."
    );
    let (code, _stdout, stderr) = eval_cli(expr);
    assert_ne!(
        code,
        Some(0),
        "a remote pid on a plain runtime must be refused\n--- stderr ---\n{stderr}"
    );
    assert!(
        stderr.contains("not a distributed node"),
        "the refusal names the missing node identity, got {stderr:?}"
    );
}

/// **`{Name, Node}` with a non-atom `Node`, and a bare unregistered name, are `badarg`.**
///
/// Oracle (probes 7 and 8): `{'EXIT',{badarg,…}}` for both — the tuple arm is tried only
/// for a 2-tuple of atoms, and an unregistered *atom* destination never reaches the tuple
/// arm at all.
#[test]
fn a_malformed_or_unregistered_destination_is_badarg() {
    for expr in ["{foo, 42} ! hi.", "{a, b, c} ! hi.", "noregistered ! hi."] {
        let (code, _stdout, stderr) = eval_cli(expr);
        assert_eq!(
            code,
            Some(1),
            "`{expr}` must raise error:badarg (exit 1)\n--- stderr ---\n{stderr}"
        );
        assert!(
            stderr.contains("badarg") && !stderr.contains("unsupported"),
            "`{expr}` is the oracle's badarg, not a capability gap, got {stderr:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// 2. The three send routes classify one destination identically
// ---------------------------------------------------------------------------------------

/// **The compiled `!`, the `erlang:send/2` BIF and the `-eval` `!` agree on a remote
/// `{Name, OtherNode}` pair.**
///
/// This is the point of the *shared* classifier: three different entry points
/// (`ExecutionContext::send_message_with_off_heap` from the `send` instruction,
/// `ExecutionContext::send_message` from the `SendBridge` BIF hook, and the `-eval`
/// evaluator's `!`) must not be able to drift.
#[test]
fn the_compiled_bang_the_bif_and_the_eval_send_agree_on_a_remote_name_pair() {
    let mut rt = runtime_with_fixture();
    let name = rt.intern("foo");
    let node = rt.intern("other@nohost");
    let hi = rt.intern("hi");

    let bang = run_fixture(&mut rt, "bang_named", vec![name, node, hi])
        .expect_err("the compiled `{foo, other@nohost} ! hi` is a capability gap");
    let bif = run_fixture(&mut rt, "bif_named", vec![name, node, hi])
        .expect_err("the `erlang:send({foo, other@nohost}, hi)` BIF is the same gap");
    assert_eq!(
        bang, bif,
        "the compiled `!` and the `erlang:send/2` BIF must classify one destination identically"
    );
    assert!(
        bang.starts_with("unsupported") && bang.contains("other@nohost"),
        "both reasons are the capability gap naming the unreachable node, got {bang:?}"
    );

    // The `-eval` route reaches the same classifier (it calls the very same
    // `RuntimeContext::send_message`), so its external signal must name the same reason.
    let (code, _stdout, stderr) = eval_cli("{foo, 'other@nohost'} ! hi.");
    assert_eq!(code, Some(3), "the eval route reports the gap (exit 3)");
    assert!(
        stderr.contains(
            "erlang:send/2 to foo@other@nohost: distributed send is not implemented"
        ),
        "the eval route's reason is the shared classifier's, got {stderr:?}"
    );
}

// ---------------------------------------------------------------------------------------
// 3. A `{Name, <this node>}` pair is a LOCAL named send (no distribution involved)
// ---------------------------------------------------------------------------------------

/// **The compiled `{Name, node()} ! Msg` reaches a local process registered under
/// `Name`.**
///
/// Oracle probe 2 (`register(foo, self()), {foo, node()} ! hi, receive …` ⇒ `got hi`),
/// which holds on a non-distributed node and on a `-sname` node alike — the same-node arm
/// never touches the link table. No peer is needed, and none is started.
#[test]
fn a_same_node_name_pair_is_a_local_named_send() {
    let mut rt = runtime_with_fixture();
    assert_eq!(
        rt.atom_name_of(make_atom(rt.local_node())),
        Some(NONODE),
        "a plain runtime's node() is the fixed {NONODE}"
    );

    // The receiver registers `remote_send_recv` and blocks waiting for one message.
    let receiver = rt
        .spawn_process("remote_send", "recv_once", vec![])
        .expect("spawn the receiver");
    rt.run_scheduler().expect("the receiver reaches its receive");

    let name = rt.intern("remote_send_recv");
    let hi = rt.intern("hi");
    let sender = rt
        .spawn_process("remote_send", "same_node", vec![name, hi])
        .expect("spawn the same-node sender");
    rt.run_scheduler().expect("the pass drains the queue");

    assert!(
        rt.process_exit(sender).is_none(),
        "a same-node named send is not an error: it is delivered (or dropped), not raised"
    );
    let got = rt
        .process_result(receiver)
        .expect("the receiver returned the message it was woken by");
    assert_eq!(
        rt.atom_name_of(got),
        Some("hi"),
        "the receiver registered as remote_send_recv got the message the compiled \
         {{Name, node()}} pair addressed it with"
    );
}

/// **A same-node pair under an unregistered name is accepted and dropped** — the C's
/// `bif.c:2654` `return 0`, which reports the message term rather than raising.
///
/// Oracle probe 3: `{noreg, node()} ! hi` ⇒ `hi`, with nothing delivered.
#[test]
fn a_same_node_pair_with_an_unregistered_name_is_accepted_and_dropped() {
    let mut rt = runtime_with_fixture();
    let hi = rt.intern("hi");
    let result = run_fixture(&mut rt, "unregistered_same_node", vec![hi])
        .expect("an unregistered same-node name is accepted, not an error");
    assert_eq!(
        rt.atom_name_of(result.expect("the send reports the message term")),
        Some("hi"),
        "the oracle answers `hi` (bif.c:2654's return 0)"
    );

    // The oracle cross-check, in one line: the same expression must answer `hi` there too.
    if let Some(oracle) = oracle() {
        let probe = "register(recv, self()), {unregistered_name, node()} ! hi, \
                     io:format(\"~p~n\", [hi]), halt(0).";
        match support::oracle_eval(&oracle, None, probe) {
            Some(stdout) => assert_eq!(stdout, "hi", "the oracle accepts and drops it too"),
            None => println!("note: the oracle probe could not answer; the parity check is skipped"),
        }
    }

    // And the *registered* half of the same route, cross-checked on the oracle.
    if let Some(oracle) = oracle() {
        let probe = "register(recv, self()), {recv, node()} ! hi, \
                     receive M -> io:format(\"got ~p~n\", [M]) \
                     after 1000 -> io:format(\"timeout~n\") end, halt(0).";
        match support::oracle_eval(&oracle, None, probe) {
            Some(stdout) => assert_eq!(
                stdout, "got hi",
                "the oracle's same-node pair is a LOCAL named send (no distribution)"
            ),
            None => println!("note: the oracle probe could not answer; the parity check is skipped"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// 4. The LIVE remote route, and its negative control
// ---------------------------------------------------------------------------------------

/// What one live session observed.
struct LiveOutcome {
    /// The peer's `GOT …` line, when it printed one.
    got: Option<String>,
    /// The runtime's own answer for the compiled send (a reason name, or `None` on a
    /// successful transmission).
    refused: Option<String>,
    /// The peer's spawn command (for failure diagnostics).
    command: String,
    /// The peer's stderr.
    stderr: String,
}

/// Runs one live session: a real `-sname` peer that registers `name` and prints every
/// message, and the **compiled** `{Name, PeerNode} ! Msg` route of this runtime.
///
/// `distributed` selects the subject (this runtime is a `-sname` node with an established
/// outbound link to the peer) or the **negative control** (a plain runtime, whose
/// classifier refuses the tuple before anything is transmitted). Everything else — the
/// peer, the fixture, the destination term and the driving of the scheduler — is
/// identical, so the only difference is whether the message could leave this runtime.
fn run_live_session(oracle: &Path, peer_node: &str, peer_name: &str, distributed: bool) -> LiveOutcome {
    let cookie = unique("k");
    let port = free_port();
    let peer = Peer::start(oracle, peer_node, peer_name, &cookie, port)
        .expect("the real `erl` peer starts");
    let Some(ready) = peer.wait_for(|line| line.contains("READY"), READY_TIMEOUT) else {
        panic!(
            "the real `erl` peer never printed its readiness line\n  command: {}\n  stderr: {}",
            peer.command,
            peer.stderr_text()
        );
    };
    assert!(ready.contains("READY"), "the peer's readiness line: {ready:?}");

    // The peer registers with `epmd` asynchronously during its own start-up, and the
    // connector resolves the port with a `PORT2_REQ` — so wait for the registration.
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

    let Some(host) = erl_hostname(oracle) else {
        panic!("the system `erl` cannot report its own short hostname");
    };
    let peer_full = format!("{peer_node}@{host}");

    let mut rt = match distributed {
        true => {
            let our_node = unique("c");
            let rt = node_runtime(&our_node, &cookie).expect("the -sname runtime starts");
            rt
        }
        false => runtime_with_fixture(),
    };
    if distributed {
        rt.load_module(FIXTURE_BEAM).expect("the fixture loads");
        rt.run_one_slice().expect("a scheduler pass");
        rt.dist_connect(peer_node)
            .unwrap_or_else(|err| panic!("dist_connect('{peer_node}') failed: {err:?}"));
        assert!(
            rt.dist_is_connected(peer_node),
            "the outbound link completed its handshake"
        );
    }

    // The compiled `!`: `{Name, PeerNode} ! 4242`, built entirely from atoms.
    let args = vec![rt.intern(peer_name), rt.intern(&peer_full), make_small(4242)];
    let refused = match run_fixture(&mut rt, "bang_named", args) {
        Ok(_) => None,
        Err(reason) => Some(reason),
    };

    // The peer's own OTP node is what decodes and routes the `DOP_REG_SEND`, so the
    // scheduler keeps stepping the link while we wait for its stdout.
    let wait = if distributed {
        MESSAGE_TIMEOUT
    } else {
        SILENCE_WINDOW
    };
    let deadline = Instant::now() + wait;
    let mut got = None;
    while Instant::now() < deadline {
        if rt.run_one_slice().is_ok() {
            // keep pumping
        }
        if let Some(line) = peer.wait_for(
            |line| line.contains("GOT ") || line.contains("IDLE"),
            Duration::from_millis(50),
        ) {
            if line.contains("GOT ") {
                got = Some(line);
            }
            break;
        }
    }

    LiveOutcome {
        got,
        refused,
        command: peer.command.clone(),
        stderr: peer.stderr_text(),
    }
}

/// **The compiled remote `{Name, PeerNode} ! Msg` reaches a real `erl` peer.**
///
/// This runtime is a `-sname` node with an established outbound link to the peer; the
/// classification routes the tuple to `SendDestination::RemoteName`, which the shared
/// `RuntimeContext::deliver_remote` transmits as `{DOP_REG_SEND, From, Cookie, ToName}`.
/// The assertion is on the peer's **own** stdout.
#[test]
fn a_compiled_bang_delivers_a_remote_name_destination_to_a_real_peer() {
    let Some(oracle) = oracle() else {
        return; // `require_oracle` already reported the loud skip
    };
    if epmd_unreachable() {
        eprintln!(
            "SKIP: the peer registers with `epmd` (and this runtime resolves it there), \
             and no daemon answers on {}",
            beam_dist::EPMD_DEFAULT_ADDR
        );
        return;
    }

    let peer_node = unique("p");
    let peer_name = unique("n");
    let outcome = run_live_session(&oracle, &peer_node, &peer_name, true);
    assert!(
        outcome.refused.is_none(),
        "the remote send must be transmitted, not refused (got {:?})",
        outcome.refused
    );
    let Some(got) = outcome.got else {
        panic!(
            "the peer never printed the message the compiled remote `!` should have sent\n  \
             command: {}\n  stderr: {}",
            outcome.command, outcome.stderr
        );
    };
    assert!(
        got.contains("4242"),
        "the peer's registered process received the payload (peer printed {got:?})"
    );
}

/// **The negative control**: the same peer, the same fixture, the same compiled `!`, but a
/// runtime with **no** `-sname`.
///
/// The classifier refuses the tuple (`dist_client()` is `None`), so nothing is transmitted
/// and the peer stays silent. Without this, the `GOT 4242` above could not be told apart
/// from "the peer prints on anything".
#[test]
fn without_a_node_the_same_tuple_is_refused_and_nothing_is_delivered() {
    let Some(oracle) = oracle() else {
        return; // `require_oracle` already reported the loud skip
    };
    if epmd_unreachable() {
        eprintln!(
            "SKIP: the control still starts a real peer (so the silence means something), \
             and no daemon answers on {}",
            beam_dist::EPMD_DEFAULT_ADDR
        );
        return;
    }

    let peer_node = unique("q");
    let peer_name = unique("m");
    let outcome = run_live_session(&oracle, &peer_node, &peer_name, false);
    let Some(refusal) = outcome.refused else {
        panic!(
            "a plain runtime must refuse a remote {{Name, Node}} destination, not transmit it\n  \
             command: {}\n  stderr: {}",
            outcome.command, outcome.stderr
        );
    };
    assert!(
        refusal.starts_with("unsupported") && refusal.contains("distributed send is not implemented"),
        "the refusal is the documented capability gap, got {refusal:?}"
    );
    assert!(
        outcome.got.is_none(),
        "the peer must print nothing: the message was refused before it could be sent (peer \
         printed {:?})",
        outcome.got
    );
}
