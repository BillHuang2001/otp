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

//! The **driver→owner completion channel**, end to end (contract A.4).
//!
//! `src/port/completion.rs` turns `beam_io`'s term-free
//! `PortCompletion` into the two messages a socket driver owes the port that
//! issued a control op — `{inet_reply, Port, ok | {error, R}}` (plus the decoded caller tag as a
//! fourth element, `inet_reply_finish`, `inet_drv.c:2614-2637`) and
//! `{inet_async, Port, Id, ok | {ok, Data} | {ok, Port2} | {error, R}}`
//! (`send_async_ok`/`send_async_ok_port`/`send_async_error`, `inet_drv.c:2493-2562`) — and adopts
//! an accepted descriptor into a real `tcp_inet` port (the runtime half of `tcp_inet_copy`,
//! `inet_drv.c:11834`). The colocated unit suite (`src/port/completion_tests.rs`) pins the
//! message *shapes*; this file pins the whole loop: a real `.beam` parked inside `prim_inet`'s
//! own `receive`, freed by a driver completion that this runtime staged and delivered.
//!
//! # Why the blocking half must run on the `run` route
//!
//! A socket operation blocks the calling Erlang process in a plain `receive`
//! (`prim_inet:recv0/3`, `prim_inet:accept0/3`). Under `-eval` the evaluator is a tree-walker with
//! no scheduler behind it, so a parked nested call folds into the typed capability gap
//! (`unsupported: opcode: run: process blocked`, exit 3) — it can never be resumed by a driver
//! completion. `beam_rt run <file.beam> <mod>:<fun>/<arity>` runs the module as a *scheduled*
//! process instead, so the target parks, a later scheduler pass drains the completion channel,
//! the message lands in its mailbox and `resume-on-send` re-enqueues it. Every blocking case below
//! is therefore driven through the `run` action, the convention `tests/port_wake.rs` established
//! for the `{Port, {data, …}}` half.
//!
//! # The three pinned paths
//!
//! | Test | Fixture | Completion pinned |
//! |---|---|---|
//! | [`a_parked_udp_receive_is_completed_by_a_later_datagram`] | `inet_completion_udp.erl` (`cpl_udp:recv/0`) | `InetAsync` `{ok, Data}` on a parked `gen_udp:recv/3` + resume-on-delivery |
//! | [`an_accepted_descriptor_is_adopted_and_its_command_is_replied`] | `cpl_tcp:go/0` (generated, see [`TCP_FIXTURE`]) | `InetAsync` `{ok, Port2}` (`Accepted{fd}` adoption), `InetAsync` `{ok, Data}` on the adopted port, `InetReply` with the decoded caller tag |
//! | [`the_non_blocking_udp_round_trip_agrees_with_the_oracle`] | `inet_completion_parity.erl` (`cpl_parity:go/0`) | the non-blocking route on **both** runtimes (regression guard) |
//!
//! # The peer is never Erlang
//!
//! `gen_tcp:connect/2,3,4` is blocked inside this runtime (`erlang:read_timer/1` has no host
//! hook) and `gen_tcp:accept/1,2` reaches `prim_inet:accept_opts/3`, whose `getopts` list
//! (`delay_send`, `priority`, `linger`, `tos`, `recvtos`, `recvttl`) this runtime's `inet` driver
//! does not resolve. Both fixtures therefore work at the `prim_inet` level — `async_accept/2`,
//! `recv/3` and an explicit `erlang:port_command/2` with `prim_inet:send/3`'s own caller-tag
//! prefix — and every *peer* is a std [`UdpSocket`]/[`TcpStream`] owned by the test, so no Erlang
//! node is needed to drive this runtime's blocking path.
//!
//! # Bounds and skips
//!
//! Nothing here can hang: every wait on a child is bounded (a timeout kills the child and panics
//! with its captured stdout and stderr), every socket read carries a timeout, and the fixtures'
//! own `recv` timeouts are 5 s. A missing toolchain is a **loud** skip
//! ([`support::skip_loudly`]), never a silent pass — `erlc` compiles the fixtures, and `erl` is
//! only needed for the two oracle halves.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// The shared harness is compiled into every test target; this one uses a subset of it, so the
// unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

use support::{find_on_path, require_oracle, resolve_oracle, run_bounded, skip_loudly};

/// The built runtime — the `run` action is what this suite drives.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The committed blocking-UDP fixture (`tests/fixtures/inet_completion_udp.erl`, module
/// `cpl_udp`): opens one passive socket, prints the port it was bound to, then blocks in
/// `gen_udp:recv/3` — the process has nothing else to run, so only the runtime's delivery of the
/// datagram's `InetAsync` completion can end the receive.
const UDP_FIXTURE: &str = include_str!("fixtures/inet_completion_udp.erl");

/// The committed non-blocking parity fixture (`tests/fixtures/inet_completion_parity.erl`,
/// module `cpl_parity`): two passive UDP sockets, a datagram sent **before** the receive is
/// entered, and two normalised printed lines — so the identical fixture runs under this runtime
/// and under the system `erl` and their answers can be compared.
const PARITY_FIXTURE: &str = include_str!("fixtures/inet_completion_parity.erl");

/// The TCP accept/recv/send fixture (module `cpl_tcp`), generated rather than committed because
/// it is only ever compiled by this suite.
///
/// It stays at the `prim_inet` level (`gen_tcp:accept/1,2` is blocked — see the module docs) and
/// prints, in order: the bound listening port, the listening **port id**, the accepted socket's
/// **port id**, the `recv` answer, and the shape of the `{inet_reply, Sock, Status, Tag}` the
/// command was answered with. `Tag` is compared against the reference the fixture itself minted,
/// so the fourth element is the *decoded* `term_to_binary(Mref, [local])` image and not an
/// invented term. Nothing is printed before the fact it reports is true, so the harness can read
/// the lines in order and drive its std peer in between.
const TCP_FIXTURE: &str = r#"%% Generated by `beam_rt/tests/inet_completion.rs` (never committed).
-module(cpl_tcp).
-export([go/0]).

go() ->
    {ok, L} = prim_inet:open(tcp, inet, stream),
    {ok, _Bound} = prim_inet:bind(L, {127, 0, 0, 1}, 0),
    {ok, {_, ListenPort}} = prim_inet:sockname(L),
    ok = prim_inet:listen(L, 5),
    io:format("tcp_port=~p~n", [ListenPort]),
    {id, ListenId} = erlang:port_info(L, id),
    io:format("listen_id=~p~n", [ListenId]),
    {ok, AcceptRef} = prim_inet:async_accept(L, -1),
    Sock = receive {inet_async, L, AcceptRef, {ok, S}} -> S end,
    {id, SockId} = erlang:port_info(Sock, id),
    io:format("sock_id=~p~n", [SockId]),
    R = prim_inet:recv(Sock, 0, 5000),
    io:format("sock_recv=~p~n", [R]),
    Mref = erlang:monitor(port, Sock),
    MrefBin = term_to_binary(Mref, [local]),
    Size = byte_size(MrefBin) band 16#FFFF,
    true = erlang:port_command(Sock, [<<Size:16, MrefBin/binary>>, <<"back">>]),
    Tag = receive
              {inet_reply, Sock, ok, T} -> {tagged, T =:= Mref};
              {inet_reply, Sock, ok} -> untagged
          after 5000 -> timeout
          end,
    erlang:demonitor(Mref, [flush]),
    io:format("reply_tag=~p~n", [Tag]),
    ok.
"#;

/// How long a run is given to print the next expected line. Generous against child start-up (the
/// runtime boots its preloaded modules and the boot script before the fixture runs, ~0.5 s) while
/// still failing fast: the fixtures' own `recv` timeouts are 5 s, so a missing completion ends the
/// run long before this bound.
const LINE_TIMEOUT: Duration = Duration::from_secs(25);

/// How long a run is given to exit once its last expected line has been read.
const EXIT_TIMEOUT: Duration = Duration::from_secs(25);

/// The bound on the test's own socket reads.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------------------------
// A scratch directory
// ---------------------------------------------------------------------------------------------

/// A per-test, self-removing scratch directory.
///
/// The name carries the label, the pid, a per-process nonce and a per-call sequence number, so two
/// tests (or two concurrently running test *processes*, or a leftover directory from an earlier
/// run) can never share a path — the rule the crate's own `context::test_support::scratch_dir`
/// follows, re-stated here because that helper is `pub(crate)` and this is a separate crate.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// The directory this guard owns.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Creates a fresh scratch directory for `label`.
fn scratch_dir(label: &str) -> Scratch {
    static NONCE: OnceLock<u64> = OnceLock::new();
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let nonce = *NONCE.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        nanos ^ (u64::from(std::process::id()) << 32)
    });
    let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "beamrt_inet_completion_{label}_{}_{nonce:016x}_{sequence}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap_or_else(|error| {
        panic!(
            "cannot create the scratch directory {}: {error}",
            path.display()
        )
    });
    Scratch { path }
}

// ---------------------------------------------------------------------------------------------
// Toolchain, fixture compilation
// ---------------------------------------------------------------------------------------------

/// The system `erlc` — the fixtures are `.erl` sources, so a missing compiler is a loud skip.
fn require_erlc(banner: &str) -> Option<PathBuf> {
    match find_on_path("erlc") {
        Some(erlc) => Some(erlc),
        None => {
            skip_loudly(
                banner,
                "no `erlc` on $PATH (the fixture cannot be compiled)",
            );
            None
        }
    }
}

/// Writes `source` into `scratch` under the module name `module` and compiles it with `erlc`,
/// returning the `.beam`'s path.
///
/// The committed fixtures are named after the *test*, not after the module they define
/// (`inet_completion_udp.erl` holds `-module(cpl_udp)`), and `erlc` refuses that mismatch
/// (`Module name 'cpl_udp' does not match file name 'inet_completion_udp'`), so every source is
/// materialised under its module name first. `tests/fixtures/` is deliberately never passed as a
/// code path: it holds a `lists.beam` that would shadow stdlib, so only this scratch directory is
/// ever named on a `-pa`/`-o`.
fn compile_fixture(erlc: &Path, scratch: &Scratch, module: &str, source: &str) -> PathBuf {
    let source_path = scratch.path().join(format!("{module}.erl"));
    std::fs::write(&source_path, source)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", source_path.display()));
    let out_dir = scratch.path().to_string_lossy().into_owned();
    let source_arg = source_path.to_string_lossy().into_owned();
    let Some(output) = run_bounded(erlc, &["-o", &out_dir, &source_arg]) else {
        panic!(
            "`erlc` could not compile {} (bounded probe)",
            source_path.display()
        );
    };
    assert!(
        output.status.success(),
        "`erlc` failed on {} with {:?}:\n{}",
        source_path.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let beam = scratch.path().join(format!("{module}.beam"));
    assert!(beam.is_file(), "`erlc` produced no {}", beam.display());
    beam
}

// ---------------------------------------------------------------------------------------------
// A bounded run of a fixture, with its stdout read line by line
// ---------------------------------------------------------------------------------------------

/// A fixture run whose stdout is read **live**, one line at a time, so the test can react to what
/// the fixture printed (send a datagram, connect a peer, write a payload) instead of only reading
/// its output after it exits.
///
/// Every wait is bounded, and an unfinished child is killed and reaped by both the timeout arms
/// and [`Drop`], so a failing assertion can never leak a running runtime — or hang the suite on an
/// open socket.
struct FixtureRun {
    child: Child,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    command: String,
    /// Every line read so far, in order (what a timeout diagnostic prints).
    seen: Vec<String>,
}

impl FixtureRun {
    /// Starts `program args…` with stdin at `/dev/null` and both output pipes drained by threads.
    fn start(program: &Path, args: &[&str]) -> std::io::Result<FixtureRun> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("the run's stdout is piped");
        let stderr_pipe = child.stderr.take().expect("the run's stderr is piped");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
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
                        Ok(mut buffer) => {
                            buffer.push_str(&line);
                            buffer.push('\n');
                        }
                        Err(_) => break,
                    },
                    Err(_) => break,
                }
            }
        });
        let command = format!(
            "{} {}",
            program.display(),
            args.iter()
                .map(|arg| arg.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(FixtureRun {
            child,
            lines,
            stderr,
            command,
            seen: Vec::new(),
        })
    }

    /// Waits (bounded) for the next stdout line satisfying `pred`, recording every line it passes.
    fn wait_for_line<F: Fn(&str) -> bool>(&mut self, pred: F, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    let matched = pred(&line);
                    self.seen.push(line.clone());
                    if matched {
                        return Some(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    return None
                }
            }
        }
    }

    /// [`FixtureRun::wait_for_line`], or a panic naming `what` with the run's captured stdout and
    /// stderr — the run is killed first, so the diagnostic is complete and nothing is left running.
    fn expect_line<F: Fn(&str) -> bool>(
        &mut self,
        pred: F,
        timeout: Duration,
        what: &str,
    ) -> String {
        match self.wait_for_line(pred, timeout) {
            Some(line) => line,
            None => {
                self.terminate();
                panic!("{what}\n{}", self.report());
            }
        }
    }

    /// Kills and reaps the child, then collects whatever its pipes still hold.
    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.drain(Duration::from_millis(500));
    }

    /// Waits (bounded) for the child to exit on its own, then collects the rest of its output.
    fn finish(mut self, timeout: Duration) -> Finished {
        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => {
                    timed_out = true;
                    break None;
                }
            }
        };
        self.drain(Duration::from_millis(500));
        Finished {
            status,
            timed_out,
            lines: std::mem::take(&mut self.seen),
            stderr: self.stderr_text(),
            command: std::mem::take(&mut self.command),
        }
    }

    /// Collects lines until the pipe closes or `timeout` elapses.
    fn drain(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => self.seen.push(line),
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// The child's stderr collected so far.
    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|buffer| buffer.clone())
            .unwrap_or_default()
    }

    /// The run's command, stdout and stderr, formatted for a failure message.
    fn report(&self) -> String {
        let mut out = format!("  command: {}\n  stdout:\n", self.command);
        for line in &self.seen {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("  stderr:\n");
        for line in self.stderr_text().lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

impl Drop for FixtureRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a finished run left behind.
struct Finished {
    status: Option<ExitStatus>,
    timed_out: bool,
    lines: Vec<String>,
    stderr: String,
    command: String,
}

impl Finished {
    /// The child's exit code, or `None` when it exited abnormally or was killed at the bound.
    fn code(&self) -> Option<i32> {
        self.status.and_then(|status| status.code())
    }

    /// Fails unless the run exited **on its own with status 0** — a fixture that raised, or one
    /// this suite had to kill, is a failure, never a pass.
    fn assert_exit_ok(&self, what: &str) {
        assert!(
            !self.timed_out,
            "{what}: the run never exited, so it was killed at the bound\n{}",
            self.report()
        );
        assert_eq!(
            self.code(),
            Some(0),
            "{what}: the run exited with {:?}\n{}",
            self.code(),
            self.report()
        );
    }

    /// The run's command, stdout and stderr, formatted for a failure message.
    fn report(&self) -> String {
        let mut out = format!("  command: {}\n  stdout:\n", self.command);
        for line in &self.lines {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("  stderr:\n");
        for line in self.stderr.lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Printed-shape helpers
// ---------------------------------------------------------------------------------------------

/// The decimal `i64` a `<prefix><number>` line carries (`udp_port=51838` ⇒ `51838`).
fn number_after(line: &str, prefix: &str) -> Option<i64> {
    line.strip_prefix(prefix)?.trim().parse().ok()
}

/// The oracle's `~p` image of `{ok, {{127,0,0,1}, PeerPort, <<"hi">>}}` — the answer a completed
/// `gen_udp:recv/3` prints once the datagram the test sent has been received.
///
/// Built piecewise rather than as one `format!` so the brace/quote escaping cannot hide a typo.
fn udp_recv_line(peer_port: u16) -> String {
    let mut line = String::from("udp_recv={ok,{{127,0,0,1},");
    line.push_str(&peer_port.to_string());
    line.push_str(",<<\"hi\">>}}");
    line
}

/// Binds a loopback UDP socket on an ephemeral port and returns it with its own port — the peer
/// the blocking-UDP fixture receives its datagram from.
fn udp_peer() -> (UdpSocket, u16) {
    let socket =
        UdpSocket::bind(("127.0.0.1", 0)).expect("the test binds its own loopback UDP socket");
    let port = socket
        .local_addr()
        .expect("the test socket's address")
        .port();
    (socket, port)
}

/// Reads until `want` bytes have arrived (or the peer closed), bounded by `timeout`.
fn read_exactly(stream: &mut TcpStream, want: usize, timeout: Duration, what: &str) -> Vec<u8> {
    stream
        .set_read_timeout(Some(timeout))
        .expect("the test stream takes a read timeout");
    let mut received = Vec::new();
    let mut chunk = [0u8; 64];
    while received.len() < want {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(bytes) => received.extend_from_slice(&chunk[..bytes]),
            Err(error) => panic!(
                "{what}: reading the peer's reply failed after {} byte(s) ({:?}): {error}",
                received.len(),
                received
            ),
        }
    }
    received
}

// ---------------------------------------------------------------------------------------------
// 1. InetAsync recv on a parked UDP socket
// ---------------------------------------------------------------------------------------------

/// Drives one `cpl_udp:recv/0` run against a datagram the test sends, and returns the `udp_recv=`
/// line it printed, the line it **had** to print, and how the run ended.
///
/// The datagram is sent only after the fixture has printed its bound port — i.e. while it is
/// blocked in `gen_udp:recv/3` — so the completion this pins cannot be missed by a race.
fn drive_udp_receive(program: &Path, args: &[&str], label: &str) -> (String, String, Finished) {
    let mut run = FixtureRun::start(program, args)
        .unwrap_or_else(|error| panic!("cannot start the {label} run: {error}"));
    let port_line = run.expect_line(
        |line| line.starts_with("udp_port="),
        LINE_TIMEOUT,
        &format!(
            "the {label} run never printed `udp_port=…`: the socket must open and `inet:port/1` \
             must answer"
        ),
    );
    let port = number_after(&port_line, "udp_port=")
        .unwrap_or_else(|| panic!("the {label} run printed {port_line:?}, not `udp_port=<port>`"));
    assert!(
        (1024..=65535).contains(&port),
        "the {label} run bound {port}, which is not an ephemeral port"
    );

    let (peer, peer_port) = udp_peer();
    peer.send_to(b"hi", ("127.0.0.1", port as u16))
        .unwrap_or_else(|error| {
            panic!("the datagram cannot be sent to the {label} run's port {port}: {error}")
        });

    let recv = run.expect_line(
        |line| line.starts_with("udp_recv="),
        LINE_TIMEOUT,
        &format!("the {label} run never printed `udp_recv=…` (its `gen_udp:recv/3` timed out)"),
    );
    let finished = run.finish(EXIT_TIMEOUT);
    (recv, udp_recv_line(peer_port), finished)
}

/// A process parked in `gen_udp:recv/3` is completed by the **driver→owner `InetAsync`
/// completion** of a datagram that arrives later, delivered into its mailbox and resumed by the
/// scheduler.
///
/// Nothing else can end this receive: the fixture sent no message to itself, no other process
/// exists, and the only thing that changed between the park and the return is the test's datagram.
/// The assertion is therefore the completion itself:
///
/// * if the drain (`PortRegistry::drain_io_completions`) or the delivery is dropped, the
///   fixture's `recv` times out and prints `udp_recv={error,timeout}` — the exact-equality
///   assertion below fails;
/// * if the completion is delivered but the parked receiver is never re-enqueued (no
///   resume-on-delivery), `udp_recv=` is never printed and [`FixtureRun::expect_line`] fails with
///   the run's stdout and stderr;
/// * if the payload were rendered wrong (a truncated or address-less read), the tuple's shape and
///   the peer's own port would not match.
///
/// When an oracle `erl` is present the identical fixture is driven the same way under it, so the
/// expected line is **compared** rather than merely recorded; without one the shared loud skip
/// banner is printed (never a silent pass).
#[test]
fn a_parked_udp_receive_is_completed_by_a_later_datagram() {
    let Some(erlc) = require_erlc("inet_completion (blocking UDP receive)") else {
        return;
    };
    let scratch = scratch_dir("udp_blocking");
    let beam = compile_fixture(&erlc, &scratch, "cpl_udp", UDP_FIXTURE);
    let beam_arg = beam.to_string_lossy().into_owned();

    // This runtime: a real `.beam` scheduled through the `run` action.
    let rust_args = ["run", beam_arg.as_str(), "cpl_udp:recv/0"];
    let (line, expected, finished) = drive_udp_receive(Path::new(BINARY), &rust_args, "rust");
    assert_eq!(
        line, expected,
        "the parked `gen_udp:recv/3` must be completed by the datagram's `InetAsync` `{{ok, Data}}` \
         completion, rendered exactly as the oracle renders it\n{}",
        finished.report()
    );
    finished.assert_exit_ok("the blocking-UDP run");

    // The oracle: the same fixture, the same std peer, the same expected shape.
    let Some(oracle) = resolve_oracle() else {
        skip_loudly(
            "inet_completion (oracle half of the blocking UDP receive)",
            "no `erl` on $PATH (and no executable $ERTS_ORACLE)",
        );
        return;
    };
    let scratch_arg = scratch.path().to_string_lossy().into_owned();
    let oracle_args = [
        "-noshell",
        "-pa",
        scratch_arg.as_str(),
        "-eval",
        "cpl_udp:recv()",
        "-s",
        "init",
        "stop",
    ];
    let (oracle_line, oracle_expected, oracle_finished) =
        drive_udp_receive(&oracle, &oracle_args, "oracle");
    assert_eq!(
        oracle_line,
        oracle_expected,
        "the oracle's own `gen_udp:recv/3` answer must be the shape this suite asserts\n{}",
        oracle_finished.report()
    );
    oracle_finished.assert_exit_ok("the oracle blocking-UDP run");
}

// ---------------------------------------------------------------------------------------------
// 2. Accepted{fd} adoption + InetAsync recv on the adopted port + InetReply with the caller tag
// ---------------------------------------------------------------------------------------------

/// A listening socket's accepted descriptor is **adopted into a new `tcp_inet` port**, that port's
/// `InetAsync` recv completes on data the peer wrote, and the command the fixture issues is
/// answered by an `{inet_reply, Port, ok, Tag}` whose fourth element is the reference the caller
/// minted.
///
/// Each assertion names the path it pins:
///
/// * `listen_id` and `sock_id` are both present and **different** — `port_info(Port, id)` of the
///   listening port and of the accepted socket. They are equal unless the `Accepted{fd}` rewrite
///   (`PortRegistry::drain_io_completions`'s `adopt_accepted`, the C's `tcp_inet_copy`) turned
///   the descriptor into a **new port**; a `{ok, Data}`-shaped answer that never adopted anything
///   would leave the fixture with no second port to print at all.
/// * `sock_recv={ok,<<"ping">>}` is the `InetAsync` `{ok, Data}` completion **on that adopted
///   port** — the bytes are the ones this test wrote, after the accept completed, so an adoption
///   that dropped the descriptor (or forgot its stream mode) fails here.
/// * `reply_tag={tagged,true}` is the `InetReply` path *and* its caller tag: the fixture prefixes
///   its `port_command/2` payload with `prim_inet:send/3`'s own
///   `<<Size:16, term_to_binary(Mref,[local])/binary>>` header and matches the fourth element of
///   `{inet_reply, Sock, ok, Tag}` against `Mref`. A 3-tuple (the decode dropped) prints
///   `untagged`; no reply at all prints `timeout`; either fails the assertion.
/// * the bytes the test reads back (`back`) are the driver's own output carrying that command, so
///   the completing path is exercised in both directions.
///
/// # Why this fixture is not run under the oracle
///
/// `prim_inet:accept_opts/3` is unreachable here (`beam_io`'s inet `getopts` does not resolve the
/// `delay_send`/`priority`/`linger`/`tos`/`recvtos`/`recvttl` items it asks for), so the fixture
/// skips it — and it is `accept_opts` that sets the accepted socket's mode on the oracle. The
/// adopted port here inherits the **listening** port's `[binary]` option instead, which is why
/// `sock_recv` carries a binary (`{ok,<<"ping">>}`) rather than the `{ok,"ping"}` an oracle run of
/// the same fixture prints. The shapes this test asserts are therefore this runtime's own; the
/// oracle comparison lives in the sibling non-blocking test.
#[test]
fn an_accepted_descriptor_is_adopted_and_its_command_is_replied() {
    let Some(erlc) = require_erlc("inet_completion (accepted-descriptor adoption)") else {
        return;
    };
    let scratch = scratch_dir("tcp_accept");
    let beam = compile_fixture(&erlc, &scratch, "cpl_tcp", TCP_FIXTURE);
    let beam_arg = beam.to_string_lossy().into_owned();

    let mut run = FixtureRun::start(
        Path::new(BINARY),
        &["run", beam_arg.as_str(), "cpl_tcp:go/0"],
    )
    .expect("the runtime starts");

    // The listening socket is bound and listening; its port is how the test reaches it.
    let port_line = run.expect_line(
        |line| line.starts_with("tcp_port="),
        LINE_TIMEOUT,
        "the fixture never printed `tcp_port=…`: `prim_inet:open/3` + `bind/3` + `sockname/1` \
         must reach the tcp_inet driver",
    );
    let listen_port = number_after(&port_line, "tcp_port=")
        .unwrap_or_else(|| panic!("the fixture printed {port_line:?}, not `tcp_port=<port>`"));
    assert!(
        (1024..=65535).contains(&listen_port),
        "the fixture bound {listen_port}, which is not an ephemeral port"
    );

    // The listening port's own id — the value the adopted socket must differ from.
    let listen_id_line = run.expect_line(
        |line| line.starts_with("listen_id="),
        LINE_TIMEOUT,
        "the fixture never printed `listen_id=…` (its `port_info(Listen, id)` did not answer)",
    );
    let listen_id = number_after(&listen_id_line, "listen_id=")
        .unwrap_or_else(|| panic!("the fixture printed {listen_id_line:?}, not `listen_id=<id>`"));

    // The accept only completes once a peer connects, so the test connects — a std socket, because
    // `gen_tcp:connect/3` needs `erlang:read_timer/1` inside this runtime.
    let mut peer = TcpStream::connect(("127.0.0.1", listen_port as u16)).unwrap_or_else(|error| {
        panic!(
            "the test cannot connect to the fixture's listening port {listen_port}: {error}\n{}",
            run.report()
        )
    });
    peer.set_nodelay(true).ok();

    // The accepted socket's id: a **different** port, i.e. the descriptor was adopted.
    let sock_id_line = run.expect_line(
        |line| line.starts_with("sock_id="),
        LINE_TIMEOUT,
        "the fixture never printed `sock_id=…`: the queued `INET_REQ_ACCEPT` produced no \
         `{inet_async, Listen, Id, {ok, Sock}}` completion",
    );
    let sock_id = number_after(&sock_id_line, "sock_id=")
        .unwrap_or_else(|| panic!("the fixture printed {sock_id_line:?}, not `sock_id=<id>`"));
    // `port_info(Port, id)` of a live port is a small positive integer; the load-bearing assertion
    // is that the two ids differ.
    assert!(
        listen_id > 0 && sock_id > 0,
        "the two port ids must be positive (`port_info(Port, id)`), got listen_id={listen_id} \
         sock_id={sock_id}"
    );
    assert_ne!(
        sock_id,
        listen_id,
        "the accepted descriptor must be adopted into a NEW port (`tcp_inet_copy`): the accepted \
         socket reports the listening port's id {listen_id}, so no port was created for it\n{}",
        run.report()
    );

    // The payload the peer wrote is what the adopted port's `InetAsync` recv returns.
    peer.write_all(b"ping")
        .expect("the payload is written to the fixture's accepted socket");
    let recv_line = run.expect_line(
        |line| line.starts_with("sock_recv="),
        LINE_TIMEOUT,
        "the fixture never printed `sock_recv=…` (its `prim_inet:recv/3` on the adopted socket \
         timed out)",
    );
    assert_eq!(
        recv_line,
        "sock_recv={ok,<<\"ping\">>}",
        "the adopted socket's `InetAsync` `{{ok, Data}}` completion must carry the bytes the peer \
         wrote\n{}",
        run.report()
    );

    // The command's reply, with the caller's tag as its fourth element.
    let tag_line = run.expect_line(
        |line| line.starts_with("reply_tag="),
        LINE_TIMEOUT,
        "the fixture never printed `reply_tag=…`: `port_command/2` was not answered",
    );
    assert_eq!(
        tag_line,
        "reply_tag={tagged,true}",
        "the `port_command/2` on the adopted socket must be answered with \
         `{{inet_reply, Sock, ok, Tag}}` whose fourth element is the reference the caller minted \
         (`untagged` = the tag was dropped, `timeout` = no reply at all)\n{}",
        run.report()
    );

    // And the reply really reached the peer.
    let reply = read_exactly(&mut peer, 4, SOCKET_TIMEOUT, "the fixture's reply");
    assert_eq!(
        reply,
        b"back",
        "the reply the fixture's `port_command/2` carried must arrive at the peer\n{}",
        run.report()
    );

    let finished = run.finish(EXIT_TIMEOUT);
    finished.assert_exit_ok("the TCP accept/recv/send run");
    assert!(
        finished.lines.iter().any(|line| line == "ok"),
        "the fixture must run to its own return (the `run` action prints `ok` for it)\n{}",
        finished.report()
    );
}

// ---------------------------------------------------------------------------------------------
// 3. The non-blocking route, against the oracle
// ---------------------------------------------------------------------------------------------

/// The lines a parity run is compared on: the two bound ports (normalised below) and the verdict.
fn comparable_lines(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| line.starts_with("udp_ports=") || line.starts_with("parity="))
        .map(str::to_string)
        .collect()
}

/// Replaces the run's own ephemeral ports with placeholders, so the two runtimes' printed shapes
/// can be compared: `udp_ports=[57655,34934]` ⇒ `udp_ports=[<A>,<B>]`.
///
/// The replacement is done on the whole `[A,B]` token — not on each number separately — because an
/// ephemeral port is a bare decimal that can be a textual substring of the other (a 4-digit `1234`
/// occurs inside a 5-digit `12345`), so a per-number replace could rewrite the wrong one. The
/// values are validated before they are discarded (both ephemeral, both distinct), so the
/// normalisation cannot hide a socket that failed to bind or two sockets on one port.
fn normalise_ports(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| match line.strip_prefix("udp_ports=") {
            None => line.clone(),
            Some(pair) => {
                let inner = pair
                    .strip_prefix('[')
                    .and_then(|rest| rest.strip_suffix(']'))
                    .unwrap_or_else(|| {
                        panic!("`udp_ports=` must be a two-element list, got {line:?}")
                    });
                let mut ports = inner.split(',');
                let first: i64 = ports
                    .next()
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or_else(|| panic!("`udp_ports=` must hold two integers, got {line:?}"));
                let second: i64 = ports
                    .next()
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or_else(|| panic!("`udp_ports=` must hold two integers, got {line:?}"));
                assert!(
                    ports.next().is_none(),
                    "`udp_ports=` must hold exactly two ports, got {line:?}"
                );
                for port in [first, second] {
                    assert!(
                        (1024..=65535).contains(&port),
                        "`udp_ports=` reported {port}, which is not an ephemeral port ({line:?})"
                    );
                }
                assert_ne!(
                    first, second,
                    "two sockets must not share one port ({line:?})"
                );
                String::from("udp_ports=[<A>,<B>]")
            }
        })
        .collect()
}

/// The **non-blocking** UDP round trip answers the same under this runtime and under the system
/// oracle: two passive sockets, a datagram sent from one to the other *before* the receive is
/// entered, and a verdict that says whether the payload arrived intact from the sender's own port.
///
/// This is the regression guard on the route that does **not** park a process: it exercises the
/// same driver→owner channel from the other side — `gen_udp:send/4`'s
/// `erlang:port_command/2` answered by an `{inet_reply, Port, ok, Tag}`, and `gen_udp:recv/3`'s
/// `InetAsync` completion, which here arrives with the datagram already queued. `prim_inet:send/3`
/// only matches the **four**-element form (`{inet_reply, S, Status, Mref}`), so dropping either
/// the reply or just its decoded tag leaves the fixture blocked in that receive: `parity=` is then
/// never printed, and the run either exits non-zero or is killed at the probe bound — either way
/// this test fails rather than mis-reporting agreement.
///
/// The only run-dependent text either side prints is the pair of bound ports, so those are
/// normalised away ([`normalise_ports`]) and everything else is compared line for line: a
/// behaviour change on either runtime shows up as a concrete differing line, not a vague failure.
#[test]
fn the_non_blocking_udp_round_trip_agrees_with_the_oracle() {
    let Some(erlc) = require_erlc("inet_completion (oracle parity)") else {
        return;
    };
    let Some(oracle) = require_oracle("inet_completion (oracle parity)") else {
        return;
    };
    let scratch = scratch_dir("parity");
    let beam = compile_fixture(&erlc, &scratch, "cpl_parity", PARITY_FIXTURE);
    let beam_arg = beam.to_string_lossy().into_owned();
    let scratch_arg = scratch.path().to_string_lossy().into_owned();

    // This runtime: the `run` route, exactly as the blocking tests use it.
    let rust_output = run_bounded(
        Path::new(BINARY),
        &["run", beam_arg.as_str(), "cpl_parity:go/0"],
    )
    .unwrap_or_else(|| panic!("the parity fixture did not finish within the probe bound"));
    assert!(
        rust_output.status.success(),
        "the parity run on this runtime exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        rust_output.status.code(),
        String::from_utf8_lossy(&rust_output.stdout),
        String::from_utf8_lossy(&rust_output.stderr)
    );
    let rust_lines = comparable_lines(&rust_output.stdout);
    assert!(
        rust_lines
            .iter()
            .any(|line| line == "parity={payload_ok,true}"),
        "this runtime's non-blocking round trip must report the datagram arrived intact from the \
         sender's own port\n--- stdout ---\n{}",
        String::from_utf8_lossy(&rust_output.stdout)
    );

    // The oracle: the same fixture, loaded from the same directory (`-pa` before `-eval`, and
    // never `tests/fixtures/`, which holds a `lists.beam` that would shadow stdlib).
    let oracle_output = run_bounded(
        &oracle,
        &[
            "-noshell",
            "-pa",
            scratch_arg.as_str(),
            "-eval",
            "cpl_parity:go()",
            "-s",
            "init",
            "stop",
        ],
    )
    .unwrap_or_else(|| panic!("the oracle parity probe did not finish within the probe bound"));
    assert!(
        oracle_output.status.success(),
        "the oracle parity run exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        oracle_output.status.code(),
        String::from_utf8_lossy(&oracle_output.stdout),
        String::from_utf8_lossy(&oracle_output.stderr)
    );
    let oracle_lines = comparable_lines(&oracle_output.stdout);
    assert!(
        oracle_lines
            .iter()
            .any(|line| line == "parity={payload_ok,true}"),
        "the oracle's own non-blocking round trip must agree — otherwise this comparison has \
         nothing to guard\n--- stdout ---\n{}",
        String::from_utf8_lossy(&oracle_output.stdout)
    );

    assert_eq!(
        normalise_ports(&rust_lines),
        normalise_ports(&oracle_lines),
        "the printed shapes of the non-blocking round trip must agree with the oracle once each \
         runtime's own ephemeral ports are normalised\n--- rust stdout ---\n{}\n--- oracle stdout ---\n{}",
        String::from_utf8_lossy(&rust_output.stdout),
        String::from_utf8_lossy(&oracle_output.stdout)
    );
}
