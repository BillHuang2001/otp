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

//! The **nested** socket route: a blocking `gen_tcp` call sat out on a route that has no
//! scheduler of its own (contract A.4).
//!
//! A passive `gen_tcp:recv/2,3` and a `gen_tcp:accept/1,2` are not synchronous requests: the
//! driver queues its own asynchronous operation and the caller parks in a plain `receive` until
//! the driver's `{inet_async, …}` lands in its mailbox. On the `run` action that parking is
//! ordinary scheduler work (`tests/inet_completion.rs`/`tests/inet_active.rs`). On a **nested**
//! route — `beam_rt -noshell -eval '<expr>'`, or the `-s`/`-run` module call that runs *inside*
//! the command-line action — the caller is not in the run queue at all, so nothing would ever
//! answer it: the wait is driven by hand, in **two** places that must both be pinned:
//!
//! | nested consumer | code | test |
//! |---|---|---|
//! | compiled code (the engine's `pump_once`) | `src/context/pump.rs::wait_for_port_event` | [`a_passive_read_completes_on_the_engines_nested_s_route`] |
//! | the `-eval` evaluator | `src/eval/mod.rs`'s bare-`receive` port branch | every `-eval` case below |
//!
//! Both loops sit out a receive that a **live port's own asynchronous operation** will answer,
//! keep pumping the runtime's port half until the completion is delivered, and bound the wait by
//! [`PORT_EVENT_DEADLINE`](crate) — a window over **consecutive no-progress rounds**, re-armed
//! only by a round that made progress. Each test here therefore asserts a *value* (never "it
//! finished"), and the value is the one the system `erl` prints for the **identical expression
//! text**.
//!
//! # The peer is never Erlang
//!
//! Every peer is a std [`TcpStream`]/[`TcpListener`] owned by the test, held in its own thread:
//! an in-VM peer cannot do a bare `receive` under `-eval` (there is no scheduler for it) and
//! cannot sleep-then-block, so it is the wrong tool for this route. The peer **stays connected
//! after it writes** — a send-then-close peer trips the escalated EOF-ordering defect and would
//! make the case lie about what it measured.
//!
//! # Synchronisation: the run's own stdout is the clock
//!
//! A payload must arrive *after* the read operation is armed, otherwise it is buffered in the
//! socket before the driver registers its interest — which is the one documented residual below.
//! So the peer never sleeps a guessed boot delay: the fixture prints a mark immediately before it
//! enters the blocking call, the harness reads that line **live**, waits [`ARM_MARGIN`], and only
//! then writes. The same expression text and the same marks are used on both runtimes.
//!
//! # Bounds, skips and the one documented divergence
//!
//! Nothing here can hang: every wait on a child is bounded (a timeout kills and reaps it and
//! panics with its captured transcript), every peer operation carries a deadline, and the
//! deadline case asserts its own wall-clock window. A missing toolchain is a **loud** skip
//! ([`support::skip_loudly`]): `erl` is needed for every oracle half and `erlc` for the one
//! `-s`-route fixture.
//!
//! [`a_read_of_already_buffered_data_on_an_accepted_socket_stalls_to_the_peer_close`] is the one
//! `#[ignore]`d case. It pins a **known** `beam_io` residual (an accepted socket whose data was
//! already buffered before the read op armed produces no port event at all, so the wait only ends
//! at EOF) by asserting today's answer *and* the oracle's, so closing the gap flips exactly one
//! test.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// The shared harness is compiled into every test target; this one uses a subset of it, so the
// unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

use support::{find_on_path, require_oracle, run_bounded, skip_loudly};

/// The built runtime under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// How long a nested run is given to print the next expected line.
///
/// Generous against child start-up (the runtime boots its preloaded modules and the boot script
/// before the action runs, ~0.9 s measured) while still failing fast: the fixture's own socket
/// operations are answered in milliseconds once the peer has written, so a missing completion
/// ends the run long before this bound.
const LINE_TIMEOUT: Duration = Duration::from_secs(25);

/// How long a nested run is given to exit once its last expected line has been read.
///
/// A `POSIX`-style hang would otherwise stall the suite: at this bound the child is killed, and
/// the case fails with the transcript it produced instead of blocking a test thread.
const EXIT_TIMEOUT: Duration = Duration::from_secs(25);

/// How long a peer's own setup (connect, or accept of the test's listener) may take.
///
/// The peer retries, because the runtime is still booting when the test starts it and is only
/// listening once the fixture's `gen_tcp:listen/2` has run.
const PEER_DEADLINE: Duration = Duration::from_secs(20);

/// How often a peer retries connect/accept.
const RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// The gap between the fixture's mark and the peer's write.
///
/// The mark is printed immediately *before* the blocking call, and arming the driver's operation
/// takes microseconds, so 400 ms is three orders of magnitude of slack: the payload therefore
/// always arrives **after** the read is armed, which is exactly the shape these cases measure.
const ARM_MARGIN: Duration = Duration::from_millis(400);

/// The lower bound on the deadline case's wall time: the wait must really have happened.
///
/// The runtime's `PORT_EVENT_DEADLINE` is 5 s of **no progress**, so anything below this cannot
/// have waited a full window. (Measured end to end, boot included: ~7 s.)
const PORT_EVENT_WALL_FLOOR: Duration = Duration::from_secs(4);

/// The upper bound on the deadline case's wall time: the typed diagnostic must arrive in bounded
/// wall time, never as a hang.
///
/// It is deliberately larger than [`EXIT_TIMEOUT`], so a regression that *hangs* is caught by
/// that bound (the child is killed and the case fails) rather than by this one silently passing
/// on a slow host.
const PORT_EVENT_WALL_BOUND: Duration = Duration::from_secs(30);

/// How long the **oracle** is given to answer the deadline case's question before it is killed.
///
/// The C runtime suspends the process unboundedly: that is the behaviour this runtime replaces
/// with a typed diagnostic, so the oracle is expected to still be running at this bound.
const ORACLE_SUSPEND_WINDOW: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------------------------------------
// The fixed ports
// ---------------------------------------------------------------------------------------------
//
// `gen_tcp:connect/3` by **hostname** is `{error,nxdomain}` here (`inet_gethost` is not wired),
// so every peer address is the `{127,0,0,1}` tuple. The listener ports are literals rather than
// `0`, because the peer is a separate process that must be told where to connect *before* the
// run starts; each case owns its own port so the cases cannot collide, and the numbers are
// chosen to be unlikely to be in use. They are deliberately **below the host's ephemeral range**
// (`/proc/sys/net/ipv4/ip_local_port_range`, 32768-60999 here), so a peer connection's
// kernel-assigned local port can never collide with another case's listener. A listener is bound
// with `{reuseaddr,true}`, so the oracle's leg can rebind the same port straight after this
// runtime's leg.

/// [`accept_answers_an_ok_port_on_the_nested_eval_route`].
const PORT_ACCEPT: u16 = 21313;
/// [`a_passive_read_of_data_sent_after_the_arm_completes_on_the_accepted_socket`].
const PORT_ACCEPTED_RECV: u16 = 21317;
/// [`a_passive_read_of_data_sent_after_the_arm_completes_on_a_connect_side_socket`].
const PORT_CONNECT_RECV: u16 = 21321;
/// [`a_write_shutdown_alone_answers_ok`].
const PORT_SHUTDOWN_ALONE: u16 = 21325;
/// [`a_write_shutdown_before_a_read_lets_the_read_through`].
const PORT_SHUTDOWN_BEFORE_READ: u16 = 21329;
/// [`a_passive_read_completes_on_the_engines_nested_s_route`].
const PORT_S_ROUTE: u16 = 21333;
/// [`a_second_read_reuses_no_operation_id_and_still_delivers`].
const PORT_SECOND_READ: u16 = 21337;
/// [`an_accept_with_no_client_ends_at_the_port_event_deadline`].
const PORT_DEADLINE: u16 = 21345;
/// [`a_read_of_already_buffered_data_on_an_accepted_socket_stalls_to_the_peer_close`].
const PORT_BUFFERED: u16 = 21349;

// ---------------------------------------------------------------------------------------------
// The fixtures: one expression (or module) per case, `$PORT` substituted per case
// ---------------------------------------------------------------------------------------------

/// `gen_tcp:accept/1` **and** `gen_tcp:accept/2`, each answering `{ok, Port}`.
///
/// Two peers are already waiting in the listener's backlog, so `accept/1` takes the first
/// connection and `accept/2` the second. Only `is_port/1` of each answer is printed: a port's
/// `~p` image is runtime-specific and would make the comparison lie.
const ACCEPT_EXPR: &str = "\
{ok,L} = gen_tcp:listen($PORT, [binary, {active,false}, {reuseaddr,true}]), \
io:format(\"listening~n\", []), \
R1 = gen_tcp:accept(L), \
io:format(\"accept1_is_port=~p~n\", [case R1 of {ok,S1} -> is_port(S1); _ -> false end]), \
R2 = gen_tcp:accept(L, 5000), \
io:format(\"accept2_is_port=~p~n\", [case R2 of {ok,S2} -> is_port(S2); _ -> false end])";

/// A passive read on an **accepted** socket, with the payload sent only after the read is armed.
const ACCEPTED_RECV_EXPR: &str = "\
{ok,L} = gen_tcp:listen($PORT, [binary, {active,false}, {reuseaddr,true}]), \
{ok,S} = gen_tcp:accept(L), \
io:format(\"accepted=~p~n\", [is_port(S)]), \
io:format(\"armed~n\", []), \
R = gen_tcp:recv(S, 2), \
io:format(\"recv=~p~n\", [R])";

/// The same read on a **connect-side** socket (the test owns the listener).
const CONNECT_RECV_EXPR: &str = "\
{ok,S} = gen_tcp:connect({127,0,0,1}, $PORT, [binary, {active,false}]), \
io:format(\"connected=~p~n\", [is_port(S)]), \
io:format(\"armed~n\", []), \
R = gen_tcp:recv(S, 2), \
io:format(\"recv=~p~n\", [R])";

/// `gen_tcp:shutdown/2` **alone**: no read follows it.
const SHUTDOWN_ALONE_EXPR: &str = "\
{ok,L} = gen_tcp:listen($PORT, [binary, {active,false}, {reuseaddr,true}]), \
{ok,S} = gen_tcp:accept(L), \
io:format(\"accepted=~p~n\", [is_port(S)]), \
io:format(\"shutdown_alone=~p~n\", [gen_tcp:shutdown(S, write)])";

/// `gen_tcp:shutdown(S, write)` **before** a read: the write half closes, the read half still
/// delivers the peer's payload.
const SHUTDOWN_BEFORE_READ_EXPR: &str = "\
{ok,L} = gen_tcp:listen($PORT, [binary, {active,false}, {reuseaddr,true}]), \
{ok,S} = gen_tcp:accept(L), \
io:format(\"shutdown=~p~n\", [gen_tcp:shutdown(S, write)]), \
io:format(\"armed~n\", []), \
R = gen_tcp:recv(S, 2), \
io:format(\"recv=~p~n\", [R])";

/// Two consecutive passive reads at the `prim_inet` level, where the driver's operation **id** is
/// visible: `{ok, Id}` is returned per issued operation, so `Id2 - Id1 =:= 1` is the
/// no-burned-id / no-ealready property. A second queued `TCP_REQ_RECV` would make the second
/// `prim_inet:async_recv/3` answer `{error,ealready}` and fail the fixture's own match.
const SECOND_READ_EXPR: &str = "\
{ok,L} = gen_tcp:listen($PORT, [binary, {active,false}, {reuseaddr,true}]), \
{ok,S} = gen_tcp:accept(L), \
io:format(\"accepted=~p~n\", [is_port(S)]), \
{ok,Id1} = prim_inet:async_recv(S, 2, 5000), \
io:format(\"armed1~n\", []), \
D1 = receive {inet_async, S, Id1, A1} -> A1 end, \
io:format(\"first=~p~n\", [D1]), \
{ok,Id2} = prim_inet:async_recv(S, 2, 5000), \
io:format(\"armed2~n\", []), \
D2 = receive {inet_async, S, Id2, A2} -> A2 end, \
io:format(\"second=~p~n\", [D2]), \
io:format(\"id_delta=~p~n\", [Id2 - Id1])";

/// An `accept` no client ever answers: the deadline case.
const DEADLINE_EXPR: &str = "\
{ok,L} = gen_tcp:listen($PORT, [binary, {active,false}, {reuseaddr,true}]), \
R = gen_tcp:accept(L), \
io:format(\"accept=~p~n\", [R])";

/// The **residual**: the peer's payload is already buffered (the fixture sleeps between the
/// accept and the read) before the read op arms.
const BUFFERED_EXPR: &str = "\
{ok,L} = gen_tcp:listen($PORT, [binary, {active,false}, {reuseaddr,true}]), \
{ok,S} = gen_tcp:accept(L), \
io:format(\"accepted=~p~n\", [is_port(S)]), \
receive after 500 -> ok end, \
io:format(\"armed~n\", []), \
R = gen_tcp:recv(S, 2), \
io:format(\"recv=~p~n\", [R])";

/// The `-s` route's fixture (module `nested_inet_recv`), generated rather than committed because
/// only this suite compiles it.
///
/// It is the [`ACCEPTED_RECV_EXPR`] shape as **compiled code**, which is what makes the engine's
/// own pump the consumer instead of the `-eval` tree-walker. Nothing is printed before the fact
/// it reports is true, so the harness can drive its peer in between.
const S_ROUTE_FIXTURE: &str = r#"%% Generated by `beam_rt/tests/inet_nested.rs` (never committed).
-module(nested_inet_recv).
-export([recv/0]).

recv() ->
    {ok, L} = gen_tcp:listen($PORT, [binary, {active, false}, {reuseaddr, true}]),
    io:format("listening~n", []),
    {ok, S} = gen_tcp:accept(L),
    io:format("accepted=~p~n", [is_port(S)]),
    io:format("armed~n", []),
    R = gen_tcp:recv(S, 2),
    io:format("recv=~p~n", [R]),
    ok.
"#;

// ---------------------------------------------------------------------------------------------
// A scratch directory (for the `-s` route's compiled fixture)
// ---------------------------------------------------------------------------------------------

/// A per-test, self-removing scratch directory.
///
/// The name carries the label, the pid, a per-process nonce and a per-call sequence number, so
/// two tests (or two concurrently running test *processes*) can never share a path — the rule the
/// crate's own `context::test_support::scratch_dir` follows, re-stated here because that helper
/// is `pub(crate)` and this is a separate crate.
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
        "beamrt_inet_nested_{label}_{}_{nonce:016x}_{sequence}",
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

/// Compiles `source` (module `module`) with `erlc` into `scratch` and returns the `.beam`'s path.
///
/// `tests/fixtures/` is deliberately never passed as a code path: it holds a `lists.beam` that
/// would shadow stdlib, so only this scratch directory is ever named on a `-pa`/`-o`.
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
// One nested run, with its stdout read live
// ---------------------------------------------------------------------------------------------

/// One nested run (this runtime's or the oracle's) whose stdout is read **live**, one line at a
/// time, so the case can react to what the run printed — that is what lets the external peer
/// write only *after* the fixture has entered its blocking call.
///
/// Every wait is bounded and a child still running when the guard drops is killed and reaped, so
/// a failing assertion can never leak a runtime process.
struct Nested {
    child: Child,
    lines: Receiver<String>,
    stderr: Receiver<String>,
    command: String,
    /// Every stdout line read so far, in order.
    seen: Vec<String>,
    started: Instant,
    finished: bool,
}

impl Nested {
    /// Starts `program args…` with stdin at `/dev/null` and both output pipes drained by threads.
    fn start(program: &Path, args: &[&str]) -> Nested {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("cannot start {}: {error}", program.display()));
        let stdout = child.stdout.take().expect("the run's stdout is piped");
        let stderr_pipe = child.stderr.take().expect("the run's stderr is piped");
        let (line_tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if line_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        // stderr is delivered as one blob at EOF, so a finished run's diagnostics are complete
        // the moment it exits (no "read it too early and race the reader thread").
        let (err_tx, stderr) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = String::new();
            for line in BufReader::new(stderr_pipe).lines() {
                match line {
                    Ok(line) => {
                        buffer.push_str(&line);
                        buffer.push('\n');
                    }
                    Err(_) => break,
                }
            }
            let _ = err_tx.send(buffer);
        });
        let command = format!(
            "{} {}",
            program.display(),
            args.iter()
                .map(|arg| arg.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        Nested {
            child,
            lines,
            stderr,
            command,
            seen: Vec::new(),
            started: Instant::now(),
            finished: false,
        }
    }

    /// This runtime on the nested `-eval` route: `beam_rt -noshell -eval <expr>`.
    fn eval(program: &Path, expr: &str, oracle: bool) -> Nested {
        let mut args = vec!["-noshell".to_string(), "-eval".to_string(), expr.to_string()];
        if oracle {
            args.extend(["-s".to_string(), "init".to_string(), "stop".to_string()]);
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Nested::start(program, &refs)
    }

    /// The nested **module-call** route: `-s <module> <function>`, which runs compiled code
    /// through the engine (so the engine's own `pump_once` is the consumer, not the evaluator).
    fn s_route(
        program: &Path,
        code_dir: &Path,
        module: &str,
        function: &str,
        oracle: bool,
    ) -> Nested {
        let dir = code_dir.to_string_lossy().into_owned();
        let mut args = vec![
            "-noshell".to_string(),
            "-pa".to_string(),
            dir,
            "-s".to_string(),
            module.to_string(),
            function.to_string(),
        ];
        // `-s init stop` is the **oracle's** terminator: this runtime's nested action exits on its
        // own once the function returns, and running `init:stop` as a second `-s` action would
        // shut the node down instead of running the case.
        if oracle {
            args.extend(["-s".to_string(), "init".to_string(), "stop".to_string()]);
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Nested::start(program, &refs)
    }

    /// Waits (bounded) for the next stdout line containing `mark`, returning it.
    ///
    /// A run that exits first, or one that stays silent past [`LINE_TIMEOUT`], fails with the
    /// transcript it produced — never a silent pass.
    fn expect(&mut self, mark: &str) -> String {
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let transcript = self.transcript();
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.finished = true;
                panic!("the run printed no {mark:?} line in time\n{transcript}");
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(mark) {
                        return line;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    let transcript = self.transcript_after_exit();
                    panic!("the run ended before printing a {mark:?} line\n{transcript}");
                }
            }
        }
    }

    /// Waits (bounded) for the run to exit and returns its complete transcript.
    ///
    /// A run that outlives [`EXIT_TIMEOUT`] is **killed** and reported as unfinished
    /// (`success == false`), which is how a hang surfaces as a failing case instead of a blocked
    /// test thread.
    fn finish(mut self) -> Finished {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break None;
                }
                Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break None;
                }
            }
        };
        let elapsed = self.started.elapsed();
        // The child has exited, so its stdout pipe is closed: the reader thread pushes whatever
        // is left and then drops its sender, which is the only way this loop ends early.
        let drain_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = drain_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => self.seen.push(line),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        let stderr = self
            .stderr
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default();
        self.finished = true;
        Finished {
            success: status.map(|status| status.success()).unwrap_or(false),
            code: status.and_then(|status| status.code()),
            lines: std::mem::take(&mut self.seen),
            stderr,
            elapsed,
            command: self.command.clone(),
        }
    }

    /// The diagnostics for a still-running child: what it printed and what it wrote to stderr.
    fn transcript(&self) -> String {
        format!(
            "--- command ---\n{}\n--- stdout so far ---\n{}",
            self.command,
            self.seen.join("\n")
        )
    }

    /// The diagnostics for a child that has already exited (so its stderr is complete).
    fn transcript_after_exit(&mut self) -> String {
        let stderr = self
            .stderr
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.finished = true;
        format!(
            "--- command ---\n{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.command,
            self.seen.join("\n"),
            stderr
        )
    }
}

impl Drop for Nested {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A finished run: its exit status, elapsed wall time and complete transcript.
struct Finished {
    /// `true` only for a clean exit; a run killed at [`EXIT_TIMEOUT`] is `false`.
    success: bool,
    /// The exit code, when the run exited by itself.
    code: Option<i32>,
    /// Every stdout line, in the order the run printed them.
    lines: Vec<String>,
    /// Everything the run wrote to stderr.
    stderr: String,
    /// Wall time from spawn to exit.
    elapsed: Duration,
    /// The command line, for diagnostics.
    command: String,
}

impl Finished {
    /// The transcript, for a failing assertion.
    fn transcript(&self) -> String {
        format!(
            "--- command ---\n{}\n--- status ---\n{:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.command,
            self.code,
            self.lines.join("\n"),
            self.stderr
        )
    }

    /// Asserts the run exited cleanly.
    fn assert_success(&self, label: &str) {
        assert!(
            self.success,
            "case {label}: the run must exit 0 — the socket call it makes is answered on both \
             runtimes\n{}",
            self.transcript()
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The external peer
// ---------------------------------------------------------------------------------------------

/// What the test asks its peer socket to do.
enum PeerCommand {
    /// Write these bytes, keeping the connection open afterwards.
    Send(Vec<u8>),
    /// Close the connection — the peer's last act.
    Close,
}

/// How the peer obtains its socket.
enum PeerRole {
    /// Dial the fixture's listener (retrying: the fixture is not listening yet when the peer
    /// starts, because the runtime is still booting).
    Connector,
    /// Accept the fixture's connect — the listener is the test's, bound synchronously before the
    /// peer thread starts, so the fixture can never dial a port that is not open yet.
    Acceptor(TcpListener),
}

/// A peer socket owned by the test, driven from a channel.
///
/// The socket lives in the peer's own thread for as long as the guard does, so "the peer holds
/// the connection open until the test says otherwise" is structural: a send-then-close peer would
/// trip the escalated EOF-ordering defect and make a case lie. Dropping the guard closes the
/// socket (and the thread is bounded, so this can never hang a test).
struct Peer {
    tx: Option<Sender<PeerCommand>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Peer {
    /// A peer that dials `port` (with a retry loop) and then holds the connection open.
    fn connect(port: u16) -> Peer {
        Peer::spawn(PeerRole::Connector, port)
    }

    /// A peer that **owns the listener** on `port` and accepts the fixture's connect.
    ///
    /// The bind happens here, synchronously, so the fixture's `gen_tcp:connect/3` (which does not
    /// retry) cannot race the peer's setup.
    fn accept(port: u16) -> Peer {
        let listener = bind(port);
        Peer::spawn(PeerRole::Acceptor(listener), port)
    }

    /// Writes `bytes` from the peer, keeping the connection open.
    fn send(&self, bytes: &[u8]) {
        if let Some(tx) = &self.tx {
            // A closed channel means the peer thread already gave up; the case then fails on its
            // missing answer, with the peer's own stderr diagnostic alongside it.
            let _ = tx.send(PeerCommand::Send(bytes.to_vec()));
        }
    }

    /// Closes the connection and joins the peer thread.
    fn close(mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(PeerCommand::Close);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Spawns the peer thread for `role` on `port`.
    fn spawn(role: PeerRole, port: u16) -> Peer {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || peer_main(port, role, rx));
        Peer {
            tx: Some(tx),
            handle: Some(handle),
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Binds `port` on the loopback interface, retrying briefly if the kernel still holds it.
fn bind(port: u16) -> TcpListener {
    let deadline = Instant::now() + PEER_DEADLINE;
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return listener,
            Err(error) => {
                if Instant::now() >= deadline {
                    panic!("the test's listener cannot bind 127.0.0.1:{port}: {error}");
                }
                thread::sleep(RETRY_INTERVAL);
            }
        }
    }
}

/// The peer's thread: obtain the socket, then serve commands until closed.
fn peer_main(port: u16, role: PeerRole, rx: Receiver<PeerCommand>) {
    let Some(stream) = establish(port, role) else {
        // Drain the channel so the guard's `close`/`drop` never blocks on a peer that never
        // connected; the case that needed this socket fails on its missing answer.
        while rx.recv().is_ok() {}
        return;
    };
    hold(stream, rx);
}

/// Connects (with a retry loop) or accepts the fixture's connection.
fn establish(port: u16, role: PeerRole) -> Option<TcpStream> {
    let deadline = Instant::now() + PEER_DEADLINE;
    match role {
        PeerRole::Connector => loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => return Some(stream),
                Err(error) => {
                    if Instant::now() >= deadline {
                        eprintln!("peer: cannot connect to 127.0.0.1:{port}: {error}");
                        return None;
                    }
                    thread::sleep(RETRY_INTERVAL);
                }
            }
        },
        PeerRole::Acceptor(listener) => {
            if let Err(error) = listener.set_nonblocking(true) {
                eprintln!("peer: cannot poll 127.0.0.1:{port}: {error}");
                return None;
            }
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // The accepted stream must block for `write_all` to be a plain write.
                        if let Err(error) = stream.set_nonblocking(false) {
                            eprintln!("peer: cannot configure the accepted socket: {error}");
                            return None;
                        }
                        return Some(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            eprintln!("peer: no connection on 127.0.0.1:{port} in time");
                            return None;
                        }
                        thread::sleep(RETRY_INTERVAL);
                    }
                    Err(error) => {
                        eprintln!("peer: cannot accept on 127.0.0.1:{port}: {error}");
                        return None;
                    }
                }
            }
        }
    }
}

/// Serves the test's commands on `stream`, then closes it.
fn hold(mut stream: TcpStream, rx: Receiver<PeerCommand>) {
    while let Ok(command) = rx.recv() {
        match command {
            PeerCommand::Send(bytes) => {
                if let Err(error) = stream.write_all(&bytes) {
                    eprintln!("peer: cannot write {} byte(s): {error}", bytes.len());
                }
                let _ = stream.flush();
            }
            PeerCommand::Close => break,
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

// ---------------------------------------------------------------------------------------------
// Printed-line assertions and the two-leg driver
// ---------------------------------------------------------------------------------------------

/// The value of the `<prefix><value>` line in `lines`, or a panic naming the whole transcript.
///
/// Each observation of a case is printed as `prefix=value` with a unique prefix, so "the value
/// this runtime printed for this observation" is unambiguous; a repeated prefix is rejected, so a
/// case cannot pass over a duplicated (or stale) line.
fn value_of<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    let mut found: Option<&str> = None;
    for line in lines {
        if let Some(value) = line.strip_prefix(prefix) {
            assert!(
                found.is_none(),
                "the run printed {prefix:?} more than once\n{lines:#?}"
            );
            found = Some(value);
        }
    }
    match found {
        Some(value) => value,
        None => panic!("the run printed no {prefix:?} line\n{lines:#?}"),
    }
}

/// Asserts `finished` printed `prefix` with exactly `want`.
fn expect(finished: &Finished, prefix: &str, want: &str) {
    assert_eq!(
        value_of(&finished.lines, prefix),
        want,
        "the {prefix:?} observation is not the oracle's own rendering\n{}",
        finished.transcript()
    );
}

/// Runs the case on both runtimes — `leg(false)` for this runtime, `leg(true)` for the oracle —
/// asserts both exited cleanly, and asserts their transcripts are **identical**.
///
/// The oracle is the same expression text (or, for the `-s` route, the same compiled fixture), so
/// a behaviour change on either side shows up as a concrete differing line rather than a vague
/// failure.
fn both_legs(label: &str, leg: impl Fn(bool) -> Finished) -> (Finished, Finished) {
    let rust = leg(false);
    rust.assert_success(label);
    let oracle = leg(true);
    oracle.assert_success(label);
    assert_eq!(
        rust.lines,
        oracle.lines,
        "case {label}: the two runtimes must print the same transcript\n--- rust ---\n{}\n\
         --- oracle ---\n{}",
        rust.transcript(),
        oracle.transcript()
    );
    (rust, oracle)
}

/// The program for a leg: this runtime's binary, or the oracle's `erl`.
fn program<'a>(is_oracle: bool, oracle: &'a Path) -> &'a Path {
    if is_oracle {
        oracle
    } else {
        Path::new(BINARY)
    }
}

/// Waits the arm margin, then writes `payload` from `peer`.
fn arm_and_send(peer: &Peer, payload: &[u8]) {
    thread::sleep(ARM_MARGIN);
    peer.send(payload);
}

// ---------------------------------------------------------------------------------------------
// 1. accept
// ---------------------------------------------------------------------------------------------

/// `gen_tcp:accept/1` and `gen_tcp:accept/2` answer `{ok, Port}` on the nested `-eval` route.
///
/// The accept is the shape the nested wait was added for: `prim_inet`'s `accept0/3` issues a
/// control op, gets an operation id back and parks in a plain `receive` for the driver's
/// `{inet_async, Port, Id, {ok, Port2}}`, which only a port round that reaches the now-ready
/// listener can produce. The oracle prints `accept1_is_port=true` and `accept2_is_port=true`.
#[test]
fn accept_answers_an_ok_port_on_the_nested_eval_route() {
    let Some(oracle) = require_oracle("inet_nested (accept)") else {
        return;
    };
    let expr = ACCEPT_EXPR.replace("$PORT", &PORT_ACCEPT.to_string());
    let out = both_legs("accept on the nested route", |is_oracle| {
        // Both peers dial up front: `accept/1` takes the first connection in the listener's
        // backlog and `accept/2` the second. Neither writes, and both stay connected until the
        // run has finished, so the accepted sockets are never closed underneath it.
        let first = Peer::connect(PORT_ACCEPT);
        let second = Peer::connect(PORT_ACCEPT);
        let finished = Nested::eval(program(is_oracle, &oracle), &expr, is_oracle).finish();
        first.close();
        second.close();
        finished
    });
    expect(&out.0, "accept1_is_port=", "true");
    expect(&out.0, "accept2_is_port=", "true");
    expect(&out.1, "accept1_is_port=", "true");
    expect(&out.1, "accept2_is_port=", "true");
}

// ---------------------------------------------------------------------------------------------
// 2. a passive read on an accepted socket
// ---------------------------------------------------------------------------------------------

/// A passive `gen_tcp:recv/2` on an **accepted** socket, with the payload written only after the
/// read op is armed, answers `{ok, <<"hi">>}` on the nested `-eval` route.
///
/// This is the canonical nested port wait: nothing is runnable while the receiver is parked, and
/// the driver completes the read when the socket becomes readable. The oracle prints
/// `recv={ok,<<"hi">>}`.
#[test]
fn a_passive_read_of_data_sent_after_the_arm_completes_on_the_accepted_socket() {
    let Some(oracle) = require_oracle("inet_nested (accepted recv)") else {
        return;
    };
    let expr = ACCEPTED_RECV_EXPR.replace("$PORT", &PORT_ACCEPTED_RECV.to_string());
    let out = both_legs("accepted recv on the nested route", |is_oracle| {
        let peer = Peer::connect(PORT_ACCEPTED_RECV);
        let mut run = Nested::eval(program(is_oracle, &oracle), &expr, is_oracle);
        run.expect("accepted=true");
        run.expect("armed");
        arm_and_send(&peer, b"hi");
        let finished = run.finish();
        peer.close();
        finished
    });
    expect(&out.0, "recv=", "{ok,<<\"hi\">>}");
    expect(&out.1, "recv=", "{ok,<<\"hi\">>}");
}

// ---------------------------------------------------------------------------------------------
// 3. a passive read on a connect-side socket
// ---------------------------------------------------------------------------------------------

/// The same read on a **connect-side** socket (the test owns the listener, the fixture dials and
/// reads) answers `{ok, <<"hi">>}` on the nested route.
///
/// The connect side is the control for case 2: the identical pre-buffered payload on this socket
/// *is* delivered (see the `#[ignore]`d residual below), so this case keeps the two paths
/// distinguishable. The oracle prints `recv={ok,<<"hi">>}`.
#[test]
fn a_passive_read_of_data_sent_after_the_arm_completes_on_a_connect_side_socket() {
    let Some(oracle) = require_oracle("inet_nested (connect-side recv)") else {
        return;
    };
    let expr = CONNECT_RECV_EXPR.replace("$PORT", &PORT_CONNECT_RECV.to_string());
    let out = both_legs("connect-side recv on the nested route", |is_oracle| {
        // The listener is the test's, bound before the fixture starts, so the fixture's
        // `gen_tcp:connect/3` (which does not retry) cannot arrive too early.
        let peer = Peer::accept(PORT_CONNECT_RECV);
        let mut run = Nested::eval(program(is_oracle, &oracle), &expr, is_oracle);
        run.expect("connected=true");
        run.expect("armed");
        arm_and_send(&peer, b"hi");
        let finished = run.finish();
        peer.close();
        finished
    });
    expect(&out.0, "connected=", "true");
    expect(&out.1, "connected=", "true");
    expect(&out.0, "recv=", "{ok,<<\"hi\">>}");
    expect(&out.1, "recv=", "{ok,<<\"hi\">>}");
}

// ---------------------------------------------------------------------------------------------
// 4. shutdown/2, alone
// ---------------------------------------------------------------------------------------------

/// `gen_tcp:shutdown(S, write)` on its own answers `ok` on the nested route.
///
/// `shutdown` is a control op answered by `{inet_reply, S, ok}`, i.e. the **non**-async half of
/// the same channel: it needs no port event at all, which is why it must keep answering while the
/// nested wait is in place. The oracle prints `shutdown_alone=ok`.
#[test]
fn a_write_shutdown_alone_answers_ok() {
    let Some(oracle) = require_oracle("inet_nested (shutdown alone)") else {
        return;
    };
    let expr = SHUTDOWN_ALONE_EXPR.replace("$PORT", &PORT_SHUTDOWN_ALONE.to_string());
    let out = both_legs("shutdown/2 alone on the nested route", |is_oracle| {
        let peer = Peer::connect(PORT_SHUTDOWN_ALONE);
        let finished = Nested::eval(program(is_oracle, &oracle), &expr, is_oracle).finish();
        peer.close();
        finished
    });
    expect(&out.0, "accepted=", "true");
    expect(&out.1, "accepted=", "true");
    expect(&out.0, "shutdown_alone=", "ok");
    expect(&out.1, "shutdown_alone=", "ok");
}

// ---------------------------------------------------------------------------------------------
// 5. shutdown/2 before a read
// ---------------------------------------------------------------------------------------------

/// `gen_tcp:shutdown(S, write)` followed by a passive read: the shutdown answers `ok` and the
/// read still delivers the peer's payload (`recv={ok,<<"hi">>}`) — a closed write half must not
/// disturb the read interest the nested wait is built on.
#[test]
fn a_write_shutdown_before_a_read_lets_the_read_through() {
    let Some(oracle) = require_oracle("inet_nested (shutdown before read)") else {
        return;
    };
    let expr = SHUTDOWN_BEFORE_READ_EXPR.replace("$PORT", &PORT_SHUTDOWN_BEFORE_READ.to_string());
    let out = both_legs("shutdown before read on the nested route", |is_oracle| {
        let peer = Peer::connect(PORT_SHUTDOWN_BEFORE_READ);
        let mut run = Nested::eval(program(is_oracle, &oracle), &expr, is_oracle);
        run.expect("shutdown=");
        run.expect("armed");
        arm_and_send(&peer, b"hi");
        let finished = run.finish();
        peer.close();
        finished
    });
    expect(&out.0, "shutdown=", "ok");
    expect(&out.1, "shutdown=", "ok");
    expect(&out.0, "recv=", "{ok,<<\"hi\">>}");
    expect(&out.1, "recv=", "{ok,<<\"hi\">>}");
}

// ---------------------------------------------------------------------------------------------
// 6. the engine's own nested pump (`-s`, compiled code)
// ---------------------------------------------------------------------------------------------

/// The same read, but on the nested **module-call** route (`-s nested_inet_recv recv`), which
/// runs **compiled** code: the consumer is the engine's `pump_once` →
/// `RuntimeContext::wait_for_port_event` (`src/context/pump.rs`), not the `-eval` tree-walker.
///
/// Both nested consumers are therefore pinned; they are separate loops over separate machines
/// (that is why they were both touched), and this case is the only one that reaches the engine's.
/// The oracle runs the same compiled fixture and prints `recv={ok,<<"hi">>}`.
#[test]
fn a_passive_read_completes_on_the_engines_nested_s_route() {
    let Some(erlc) = find_on_path("erlc") else {
        skip_loudly(
            "inet_nested (-s route)",
            "no `erlc` on $PATH (the `-s` fixture cannot be compiled)",
        );
        return;
    };
    let Some(oracle) = require_oracle("inet_nested (-s route)") else {
        return;
    };
    let scratch = scratch_dir("s_route");
    let source = S_ROUTE_FIXTURE.replace("$PORT", &PORT_S_ROUTE.to_string());
    let beam = compile_fixture(&erlc, &scratch, "nested_inet_recv", &source);
    assert!(beam.is_file());

    let out = both_legs("-s route recv on the nested route", |is_oracle| {
        let peer = Peer::connect(PORT_S_ROUTE);
        let mut run = Nested::s_route(
            program(is_oracle, &oracle),
            scratch.path(),
            "nested_inet_recv",
            "recv",
            is_oracle,
        );
        run.expect("listening");
        run.expect("accepted=true");
        run.expect("armed");
        arm_and_send(&peer, b"hi");
        let finished = run.finish();
        peer.close();
        finished
    });
    expect(&out.0, "accepted=", "true");
    expect(&out.1, "accepted=", "true");
    expect(&out.0, "recv=", "{ok,<<\"hi\">>}");
    expect(&out.1, "recv=", "{ok,<<\"hi\">>}");
}

// ---------------------------------------------------------------------------------------------
// 7. no double consume
// ---------------------------------------------------------------------------------------------

/// Two consecutive passive reads on one socket: the second operation's id is the first's **+1**
/// and it delivers the peer's second payload.
///
/// This is the no-double-consume property of the nested wait. The wait consumes nothing — it
/// re-scans the mailbox at the reported `(func, pc)` — so the driver holds **no** queued async op
/// for the port once a read has completed: a second `TCP_REQ_RECV` would have made
/// `prim_inet:async_recv/3` answer `{error,ealready}` (failing this fixture's own match), and a
/// burned operation id would show up as a delta of more than one. The ids are per port, so
/// `id_delta=1` is comparable between runtimes while the raw ids are not printed.
///
/// The oracle prints `first={ok,<<"hi">>}`, `second={ok,<<"ho">>}` and `id_delta=1`.
#[test]
fn a_second_read_reuses_no_operation_id_and_still_delivers() {
    let Some(oracle) = require_oracle("inet_nested (second read)") else {
        return;
    };
    let expr = SECOND_READ_EXPR.replace("$PORT", &PORT_SECOND_READ.to_string());
    let out = both_legs("second read on the nested route", |is_oracle| {
        let peer = Peer::connect(PORT_SECOND_READ);
        let mut run = Nested::eval(program(is_oracle, &oracle), &expr, is_oracle);
        run.expect("accepted=true");
        run.expect("armed1");
        arm_and_send(&peer, b"hi");
        run.expect("first=");
        run.expect("armed2");
        arm_and_send(&peer, b"ho");
        let finished = run.finish();
        peer.close();
        finished
    });
    expect(&out.0, "first=", "{ok,<<\"hi\">>}");
    expect(&out.1, "first=", "{ok,<<\"hi\">>}");
    expect(&out.0, "second=", "{ok,<<\"ho\">>}");
    expect(&out.1, "second=", "{ok,<<\"ho\">>}");
    expect(&out.0, "id_delta=", "1");
    expect(&out.1, "id_delta=", "1");
}

// ---------------------------------------------------------------------------------------------
// 8. the deadline
// ---------------------------------------------------------------------------------------------

/// An `accept` no client ever answers ends at the port-event deadline with the **typed** gap —
/// exit 3 and `beam_rt: unsupported: port event: …` on stderr — in bounded wall time.
///
/// The bound is the point of the case: the C suspends the process unboundedly, so a regression
/// here would be a hang of the single-threaded runtime. Three bounds are asserted, each with its
/// own job:
///
/// * [`EXIT_TIMEOUT`] (inside [`Nested::finish`]) **kills** a hung run, so this test can never
///   block a test thread — a regression fails the case with a transcript instead of hanging;
/// * [`PORT_EVENT_WALL_FLOOR`] proves the wait really happened (the deadline is 5 s of *no
///   progress*, so an immediate `unsupported` would be a different, wrong behaviour);
/// * [`PORT_EVENT_WALL_BOUND`] is the "bounded wall time" claim itself.
///
/// The oracle is asked the **same question** and is expected to still be running at
/// [`ORACLE_SUSPEND_WINDOW`] — that is the cross-check for this case: the divergence is the typed
/// diagnostic replacing an unbounded suspension, not a disagreement about an answer.
#[test]
fn an_accept_with_no_client_ends_at_the_port_event_deadline() {
    let Some(oracle) = require_oracle("inet_nested (deadline)") else {
        return;
    };
    let expr = DEADLINE_EXPR.replace("$PORT", &PORT_DEADLINE.to_string());

    // No peer at all: the listener's accept interest stays armed and is never satisfied.
    let finished = Nested::eval(Path::new(BINARY), &expr, false).finish();
    assert_eq!(
        finished.code,
        Some(3),
        "a nested wait the driver never answers is the *typed* capability gap (exit 3), never a \
         hang and never a wrong answer\n{}",
        finished.transcript()
    );
    assert!(
        finished.stderr.contains("unsupported: port event:"),
        "the diagnostic must name the port-event deadline\n{}",
        finished.transcript()
    );
    assert!(
        finished.stderr.contains("within the 5 s bound"),
        "the diagnostic must state the bound it measured\n{}",
        finished.transcript()
    );
    assert!(
        finished.stderr.contains("the C waits for ever"),
        "the diagnostic must state what the oracle does instead\n{}",
        finished.transcript()
    );
    assert!(
        finished.elapsed >= PORT_EVENT_WALL_FLOOR,
        "the wait must really run for a full no-progress window (measured {:?}) — an immediate \
         `unsupported` would be a different behaviour, not this one\n{}",
        finished.elapsed,
        finished.transcript()
    );
    assert!(
        finished.elapsed <= PORT_EVENT_WALL_BOUND,
        "the deadline must arrive in bounded wall time; measured {:?} exceeds the {}s bound\n{}",
        finished.elapsed,
        PORT_EVENT_WALL_BOUND.as_secs(),
        finished.transcript()
    );

    // The oracle: the same accept, with no client either. It must NOT answer within the window —
    // the C suspends the process for ever, which is exactly what the typed gap replaces.
    let args = ["-noshell", "-eval", expr.as_str(), "-s", "init", "stop"];
    let verdict = still_running_after(&oracle, &args, ORACLE_SUSPEND_WINDOW);
    match verdict {
        Verdict::StillRunning => {}
        Verdict::Exited(code) => panic!(
            "the oracle answers the same accept by suspending the process unboundedly; it must \
             still be running after {}s, but it exited with {code:?} — this runtime's typed gap \
             would then no longer be the divergence it documents",
            ORACLE_SUSPEND_WINDOW.as_secs()
        ),
    }
}

/// How a bounded "is it still running?" probe ended.
#[derive(Debug)]
enum Verdict {
    /// The child exited on its own, with this code.
    Exited(Option<i32>),
    /// The child was still running at the bound, and was killed and reaped.
    StillRunning,
}

/// Runs `program args…` for at most `window`; returns whether it was still running.
///
/// The child's output is discarded (both pipes to `/dev/null`) so a chatty program cannot fill a
/// pipe buffer and block; it is always killed and reaped before returning.
fn still_running_after(program: &Path, args: &[&str], window: Duration) -> Verdict {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start {}: {error}", program.display()));
    let deadline = Instant::now() + window;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Verdict::Exited(status.code()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Verdict::StillRunning;
            }
            Err(error) => {
                eprintln!("warning: cannot poll {}: {error}", program.display());
                let _ = child.kill();
                let _ = child.wait();
                return Verdict::Exited(None);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 9. the documented residual (ignored)
// ---------------------------------------------------------------------------------------------

/// **Known gap (ignored):** a passive read on an **accepted** socket whose payload was already
/// buffered *before* the read op armed produces no port event at all.
///
/// This runtime's answer today is `{error,closed}`, and it only arrives when the peer closes:
/// nothing wakes the nested wait, the read interest the driver should have registered never
/// fires, and the wait's own no-progress deadline is not reached because the *listener's*
/// interest keeps the round busy until EOF. The oracle answers `{ok,<<"hi">>}` immediately, from
/// the buffer.
///
/// | leg | printed answer |
/// |---|---|
/// | this runtime | `recv={error,closed}` (after the peer closes) |
/// | the oracle | `recv={ok,<<"hi">>}` (immediately) |
///
/// The defect is in `beam_io`'s **accepted-socket** path (`TcpInetDriver::from_accepted` /
/// read-interest registration; the C attempts the read synchronously at control time,
/// `inet_drv.c:12309`), not in this crate's nested wait: the connect-side socket with the same
/// pre-buffered payload is delivered
/// ([`a_passive_read_of_data_sent_after_the_arm_completes_on_a_connect_side_socket`]), and the
/// only thing that differs is which port adopted the descriptor. It is a **typed mismatch after a
/// silent stall, never a hang**: both runtimes exit 0.
///
/// Both legs are asserted, so closing the gap in `beam_io` flips exactly this test (un-ignore it
/// at the same time) and no other case has to be weakened. Run it with
/// `--ignored --test-threads=1`.
#[test]
#[ignore = "beam_io: an accepted socket's already-buffered payload raises no read event"]
fn a_read_of_already_buffered_data_on_an_accepted_socket_stalls_to_the_peer_close() {
    let Some(oracle) = require_oracle("inet_nested (buffered residual)") else {
        return;
    };
    let expr = BUFFERED_EXPR.replace("$PORT", &PORT_BUFFERED.to_string());

    // This runtime: the peer writes as soon as it is connected — i.e. while the fixture is still
    // inside `accept/1`, and certainly before the 500 ms sleep is over — so the payload is in the
    // socket buffer before `gen_tcp:recv/2` is called. The answer only comes when the peer
    // closes.
    let rust = {
        let peer = Peer::connect(PORT_BUFFERED);
        peer.send(b"hi");
        let mut run = Nested::eval(Path::new(BINARY), &expr, false);
        run.expect("accepted=true");
        run.expect("armed");
        thread::sleep(ARM_MARGIN);
        peer.close();
        run.finish()
    };
    rust.assert_success("buffered residual (this runtime)");
    expect(&rust, "recv=", "{error,closed}");

    // The oracle: the same buffered payload, answered from the buffer without any close.
    let oracle_run = {
        let peer = Peer::connect(PORT_BUFFERED);
        peer.send(b"hi");
        let mut run = Nested::eval(&oracle, &expr, true);
        run.expect("accepted=true");
        run.expect("armed");
        // It answers on its own: no close, no prompt.
        run.expect("recv=");
        let finished = run.finish();
        peer.close();
        finished
    };
    oracle_run.assert_success("buffered residual (the oracle)");
    expect(&oracle_run, "recv=", "{ok,<<\"hi\">>}");

    assert_ne!(
        rust.lines, oracle_run.lines,
        "the residual is a divergence: once both sides print the same transcript the gap is \
         closed and this test should be un-ignored and its expectation flipped\n--- rust ---\n{}\n\
         --- oracle ---\n{}",
        rust.transcript(),
        oracle_run.transcript()
    );
}
