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

//! **Two-VM distribution** (contract A.5, the inbound direction): a real system-ERTS
//! node connects to this Rust ERTS runtime *and actually uses it*.
//!
//! The other A.5 suites each pin one half of a link from a different angle —
//! [`tests/dist.rs`](../dist.rs) the `'N'` handshake, [`tests/dist_client.rs`](../dist_client.rs)
//! this runtime dialling out, [`tests/live_inbound.rs`](../live_inbound.rs) an inbound
//! `DOP_REG_SEND` into a running local process, [`tests/live_signals.rs`](../live_signals.rs)
//! the outbound signal families — all of them over the **default** `epmd` port, and all of
//! them from *one* VM's point of view. This suite is the integration proof: **two VMs at
//! once**, a real system `erl` and this runtime, sharing one **private** `epmd`, and the
//! traffic that crosses between them is asserted on **both** sides.
//!
//! What each test asserts, end to end:
//!
//! 1. the runtime boots as a `-sname` node, registers with the private `epmd`, and its
//!    fixture prints a `READY <node>` line once its registered name exists;
//! 2. the system `erl` peer resolves it **by name** — `net_adm:ping('rust…@host')` answers
//!    **`pong`** (which is a real `gen:call` to the remote `net_kernel`, so the runtime has
//!    to serve that endpoint, not merely accept a socket);
//! 3. **one message each way**: the peer `DOP_REG_SEND`s `{greeting, PeerNode, 42}` to the
//!    runtime's registered name `two_vm_peer` and the runtime `!`s
//!    `{reply, RustNode, 43}` to the peer's registered name `oracle_peer`; **both** payloads
//!    are printed by the receiving VM and compared, verbatim, to the exact expected term;
//! 4. **`nodedown`** in both directions — the runtime's own `erlang:monitor_node(PeerNode,
//!    true)` reports `{nodedown, PeerNode}` when the peer exits
//!    ([`the_oracle_pings_the_rust_node_and_a_message_crosses_each_way`]), and the peer's
//!    `erlang:monitor_node(RustNode, true)` reports `{nodedown, RustNode}` when the runtime
//!    is killed ([`the_peer_watches_the_rust_node_go_down`]).
//!
//! # Running it
//!
//! Both tests are `#[ignore]`d: they need a whole system Erlang toolchain (`erl`, `erlc`,
//! `epmd`) plus `setsid` and `kill`, they take ~10 s each, and they really do spawn two
//! VMs. The exact command:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_rt --test dist_two_vm -- --ignored --nocapture
//! ```
//!
//! To run one of them: append `--exact <test-name>` after the `--`. `--nocapture` matters:
//! every child's transcript is echoed on failure *and* on every loud skip.
//!
//! # Isolation (why `setsid`, and why a *private* `epmd`)
//!
//! An Erlang VM signals its **process group** (`SIGUSR1` for the break handler, and
//! `erl_child_setup` does the same on the way down), so a VM started as a plain child of the
//! test process can hit the test binary — and any sibling test — with its own signals.
//! Every child here (the private `epmd`, the `erl` peer, the `beam_rt` runtime) is therefore
//! started as `setsid -w <program> …`: its own **session**, so its own process group, whose
//! id is its own pid. Drop sends `TERM` to the whole group, waits a bounded grace, escalates
//! to `KILL`, and reaps — so no `beam_rt`, `erl` or `epmd` is ever left behind.
//! (`setsid` execs the program directly, so the child we hold *is* the program; the group id
//! is read back from `/proc/<pid>/stat` and only trusted when it really is `<pid>` — see
//! [`ChildVm::pgid`].)
//!
//! A private `epmd` on a private port (`ERL_EPMD_PORT=<port> epmd -daemon -port <port>`,
//! with its cwd inside the test's own scratch dir) keeps the run out of any other node's way
//! — the developer's own `epmd`, and any other test's — and lets the whole pair collide with
//! nothing. A daemonized `epmd` detaches into its own session, so the group kill cannot
//! reach it: [`PrivateEpmd::stop`] finds it by its own argv (`-port <our port>`) and signals
//! that pid, then proves the port is closed.
//!
//! # Skip policy
//!
//! Every precondition is a **loud skip** (`support::skip_loudly`, or a `SKIP: …` line when
//! the reason is a *runtime* one): a missing `erl`/`erlc`/`epmd`/`setsid`/`kill` (discovered
//! by **filesystem only**, never by running the tool — see `tests/support/mod.rs`), a
//! private `epmd` that does not come up, or a runtime that never prints its `ready` line
//! inside the bound. A missing toolchain is never a silent pass, and a *missing committed
//! fixture* cannot happen here: the fixture is compiled at test time with the system `erlc`
//! into the scratch directory.
//!
//! # Assumptions the implementation must satisfy
//!
//! This file is the executable statement of what "two VMs" means; the runtime side has to
//! hold up its end. Documented here so a failure is attributable, not mysterious:
//!
//! * **`ERL_EPMD_PORT` is honoured** by the runtime — for its own `ALIVE2` registration on
//!   the first scheduler pass — so the node appears in *this* `epmd` and nowhere else.
//! * **the `net_kernel` endpoint is served**: an inbound `{is_auth, Node}` addressed to the
//!   registered name `net_kernel` is answered, which is what `net_adm:ping/1` returns
//!   `pong` from.
//! * **an accepted link can send**: `{Name, PeerNode} ! Msg` on a link *the peer opened*
//!   reaches the peer's registered name (today the documented runtime send path is the
//!   outbound/connector link).
//! * **`erlang:monitor_node(Node, true)`** answers `true` for a connected node and delivers
//!   `{nodedown, Node}` to the monitoring process when the link dies, in **both**
//!   directions.
//! * the fixture's `receive … after T` keeps the run's scheduler pump stepping (a bare
//!   `receive` leaves the pump quiescent, which is why every wait here is deadline-bounded).

use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// The shared harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// How long the private `epmd` may take to accept its first connection.
const EPMD_START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the runtime may take to print its `READY` line (it drives the whole boot script
/// first, then loads and starts the fixture).
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long any single expected stdout line may take to appear once its predecessor has.
///
/// Deliberately **longer** than every deadline the peer's expressions carry (a 100 × 200 ms
/// ping budget, a 20 s nodedown wait), so a peer that gives up prints its own `PINGFAIL` /
/// `NODEDOWN_TIMEOUT` line into the transcript *before* this wait expires — a red test then
/// names the peer's verdict instead of only "no line".
const LINE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a VM may take to exit once the interaction has completed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// The grace a `TERM`ed process group gets before it is `KILL`ed.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// The poll interval for every bounded wait in this file.
const POLL: Duration = Duration::from_millis(20);
/// How long the killed `epmd` gets to release its port.
const EPMD_STOP_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------------------
// The fixture (compiled at test time with the system `erlc`; never committed)
// ---------------------------------------------------------------------------------------

/// The runtime-side fixture. `run/1` is the full exchange of
/// [`the_oracle_pings_the_rust_node_and_a_message_crosses_each_way`]; `linger/0` is the
/// passive side of [`the_peer_watches_the_rust_node_go_down`].
///
/// Every wait is **deadline-bounded** (`after 30000` / `after 100`), and that is
/// load-bearing rather than defensive: a bare `receive` leaves the run action's scheduler
/// pump with an empty run queue, nothing to poll and no pending timer, so
/// `Runtime::run_scheduled` stops with its honest `blocked` diagnostic before the peer's
/// message can arrive (`tests/dist.rs` and `tests/live_inbound.rs` pin the same rule).
///
/// The peer's node atom arrives as a **charlist** (`list_to_atom/1`) rather than as a bare
/// `run` argument, so the command line never depends on the atom charset.
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/dist_two_vm.rs` (never committed).
-module(two_vm).
-export([run/1, linger/0]).

%% The whole two-VM exchange, from this runtime's side: register a name, take one message
%% from the peer, answer it on the peer's registered name, then watch the peer go down.
run(OracleName) ->
    Oracle = list_to_atom(OracleName),
    true = register(two_vm_peer, self()),
    io:format("READY ~p~n", [node()]),
    Got =
        receive
            M -> M
        after 30000 ->
            io:format("INBOUND_TIMEOUT~n", []),
            halt(4)
        end,
    io:format("INBOUND ~p~n", [Got]),
    {oracle_peer, Oracle} ! {reply, node(), 43},
    io:format("REPLY_SENT ~p~n", [Oracle]),
    Watching = erlang:monitor_node(Oracle, true),
    io:format("MONITORING ~p~n", [Watching]),
    receive
        {nodedown, Down} ->
            io:format("NODEDOWN ~p~n", [Down]),
            done
    after 30000 ->
        io:format("NODEDOWN_TIMEOUT~n", []),
        halt(5)
    end.

%% Stay up (and registered with `epmd`) while the peer monitors us, then goes down.
linger() ->
    io:format("READY ~p~n", [node()]),
    linger(400).

linger(0) ->
    done;
linger(N) ->
    receive _ -> ok after 100 -> ok end,
    linger(N - 1).
"#;

/// The fixture's module name (it must be what the `run` spec names).
const FIXTURE_MODULE: &str = "two_vm";

/// The runtime's entry point for the full exchange.
const EXCHANGE_SPEC: &str = "two_vm:run/1";

/// The runtime's entry point for the passive nodedown case.
const LINGER_SPEC: &str = "two_vm:linger/0";

// ---------------------------------------------------------------------------------------
// The peer's expressions
// ---------------------------------------------------------------------------------------

/// The system-`erl` side of the full exchange. `@TARGET@` is replaced by the runtime's
/// **full** `node@host` atom.
///
/// It pings the runtime until `net_adm:ping/1` answers `pong` (that call is a real
/// `gen:call` to the *remote* `net_kernel`, `kernel/src/net_adm.erl:101-110`), registers
/// `oracle_peer`, then re-sends the greeting on a bounded schedule until the reply arrives
/// (the greeting can legitimately land in the window between the runtime's `epmd`
/// registration and its fixture's `register/2`, where a `DOP_REG_SEND` to an unregistered
/// name is silently dropped — by the oracle too). Finally it halts, which is what the
/// runtime's node monitor must report.
const ORACLE_EXCHANGE_EVAL: &str = r#"
Target = list_to_atom("@TARGET@"),
Wait = fun F(0) -> io:format("PINGFAIL~n", []), halt(2);
          F(N) -> case net_adm:ping(Target) of
                      pong -> ok;
                      _ -> timer:sleep(200), F(N - 1)
                  end
       end,
Wait(100),
io:format("PONG ~p~n", [node()]),
register(oracle_peer, self()),
Greet = fun G(0) -> io:format("NOREPLY~n", []), halt(3);
           G(N) -> {two_vm_peer, Target} ! {greeting, node(), 42},
                   receive Msg -> io:format("GOT ~p~n", [Msg])
                   after 250 -> G(N - 1)
                   end
        end,
Greet(40),
io:format("ORACLE_DONE~n", []),
halt(0).
"#;

/// The system-`erl` side of the passive nodedown case: ping, then `monitor_node/2` the
/// runtime and wait for the `{nodedown, Node}` the kill must produce.
const ORACLE_WATCH_EVAL: &str = r#"
Target = list_to_atom("@TARGET@"),
Wait = fun F(0) -> io:format("PINGFAIL~n", []), halt(2);
          F(N) -> case net_adm:ping(Target) of
                      pong -> ok;
                      _ -> timer:sleep(200), F(N - 1)
                  end
       end,
Wait(100),
io:format("PONG ~p~n", [node()]),
io:format("WATCHING ~p~n", [erlang:monitor_node(Target, true)]),
receive
    {nodedown, Down} ->
        io:format("NODEDOWN ~p~n", [Down]),
        halt(0)
after 20000 ->
    io:format("NODEDOWN_TIMEOUT~n", []),
    halt(4)
end.
"#;

// ---------------------------------------------------------------------------------------
// The two tests
// ---------------------------------------------------------------------------------------

/// **A real system-ERTS node connects to this runtime and uses it.** The peer resolves the
/// runtime's node through a *private* `epmd`, `net_adm:ping/1`s it (**`pong`**), sends one
/// message to the runtime's registered name, receives one back on its own registered name,
/// and finally exits — which the runtime's `monitor_node/2` reports as `{nodedown, Peer}`.
///
/// Every observable line on both sides is compared verbatim, so a diverging payload term, a
/// missing `pong`, a lost message or a missing `nodedown` all fail here with the full
/// transcript of both VMs.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn the_oracle_pings_the_rust_node_and_a_message_crosses_each_way() {
    let Some(harness) = Harness::new("exchange") else {
        return; // every precondition already reported its loud skip
    };
    let Some(fixture) = harness.compile_fixture() else {
        return;
    };

    let rust_name = unique('r');
    let peer_name = unique('o');
    let cookie = unique_cookie();
    let rust_node = format!("{rust_name}@{}", harness.host);
    let peer_node = format!("{peer_name}@{}", harness.host);

    // The runtime first: it must be up (and `READY`) before the peer's greeting can be
    // delivered rather than dropped.
    let mut runtime = harness
        .spawn_runtime(&rust_name, &cookie, &fixture, EXCHANGE_SPEC, Some(&peer_node))
        .unwrap_or_else(|error| panic!("cannot start {BINARY} under setsid: {error}"));

    let Some(ready) = runtime.wait_for(|line| line.starts_with("READY "), READY_TIMEOUT) else {
        support::skip_loudly(
            "dist_two_vm",
            &format!(
                "the runtime printed no `READY` line within {READY_TIMEOUT:?}\n  command: {}\n  \
                 stdout: {:?}\n  stderr: {}",
                runtime.command,
                runtime.transcript(),
                runtime.stderr_text()
            ),
        );
        return;
    };
    assert_eq!(
        ready.trim(),
        format!("READY {rust_node}"),
        "the runtime's fixture reported its own full node name\n  stderr: {}",
        runtime.stderr_text()
    );
    harness.report_registration(&rust_name);

    // The peer: `net_adm:ping/1` must answer `pong`, i.e. the runtime really serves the
    // remote `net_kernel` endpoint the call is a `gen:call` to.
    let mut peer = harness
        .spawn_peer(&peer_name, &cookie, ORACLE_EXCHANGE_EVAL, &rust_node)
        .unwrap_or_else(|error| panic!("cannot start the system `erl` peer under setsid: {error}"));

    let Some(pong) = peer.wait_for(|line| line.starts_with("PONG "), LINE_TIMEOUT) else {
        panic!(
            "`net_adm:ping('{rust_node}')` never answered `pong`\n  peer command: {}\n  peer \
             stdout: {:?}\n  peer stderr: {}\n  runtime command: {}\n  runtime stdout: {:?}\n  \
             runtime stderr: {}",
            peer.command,
            peer.transcript(),
            peer.stderr_text(),
            runtime.command,
            runtime.transcript(),
            runtime.stderr_text(),
        )
    };
    assert_eq!(
        pong.trim(),
        format!("PONG {peer_node}"),
        "the peer answered only after a real `pong` from {rust_node}"
    );

    // **One message each way**, compared term for term.
    let inbound = runtime
        .wait_for(|line| line.starts_with("INBOUND "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the runtime never reported the peer's message\n  runtime stdout: {:?}\n  runtime \
                 stderr: {}\n  peer stdout: {:?}\n  peer stderr: {}",
                runtime.transcript(),
                runtime.stderr_text(),
                peer.transcript(),
                peer.stderr_text(),
            )
        });
    assert_eq!(
        inbound.trim(),
        format!("INBOUND {{greeting,{peer_node},42}}"),
        "the message the peer sent to the runtime's registered name arrived intact"
    );
    let reply_sent = runtime
        .wait_for(|line| line.starts_with("REPLY_SENT "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the runtime never reported sending its reply\n  runtime stdout: {:?}\n  runtime \
                 stderr: {}\n  peer stdout: {:?}\n  peer stderr: {}",
                runtime.transcript(),
                runtime.stderr_text(),
                peer.transcript(),
                peer.stderr_text(),
            )
        });
    assert_eq!(
        reply_sent.trim(),
        format!("REPLY_SENT {peer_node}"),
        "the runtime's compiled `!` used the peer's own node atom"
    );
    let got = peer
        .wait_for(|line| line.starts_with("GOT "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the peer never received the runtime's reply\n  peer stdout: {:?}\n  peer \
                 stderr: {}\n  runtime stdout: {:?}\n  runtime stderr: {}",
                peer.transcript(),
                peer.stderr_text(),
                runtime.transcript(),
                runtime.stderr_text(),
            )
        });
    assert_eq!(
        got.trim(),
        format!("GOT {{reply,{rust_node},43}}"),
        "the message the runtime sent to the peer's registered name arrived intact"
    );

    // The runtime's own node monitor, armed before the peer exits.
    let monitoring = runtime
        .wait_for(|line| line.starts_with("MONITORING "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the runtime never reported `erlang:monitor_node/2`\n  runtime stdout: {:?}\n  \
                 runtime stderr: {}",
                runtime.transcript(),
                runtime.stderr_text(),
            )
        });
    assert_eq!(
        monitoring.trim(),
        "MONITORING true",
        "`erlang:monitor_node(<peer>, true)` answered `true` for the connected peer"
    );

    // The peer goes away on its own, and the runtime's monitor must report it.
    if peer
        .wait_for(|line| line.trim() == "ORACLE_DONE", LINE_TIMEOUT)
        .is_none()
    {
        panic!(
            "the peer never finished its exchange\n  peer stdout: {:?}\n  peer stderr: {}\n  \
             runtime stdout: {:?}\n  runtime stderr: {}",
            peer.transcript(),
            peer.stderr_text(),
            runtime.transcript(),
            runtime.stderr_text(),
        )
    }
    let peer_status = peer
        .wait_exit(EXIT_TIMEOUT)
        .unwrap_or_else(|| panic!("the peer did not exit within {EXIT_TIMEOUT:?}"));
    assert_eq!(
        peer_status.code(),
        Some(0),
        "the peer completed the exchange and halted 0\n  peer stdout: {:?}\n  peer stderr: {}",
        peer.transcript(),
        peer.stderr_text(),
    );

    let nodedown = runtime
        .wait_for(|line| line.starts_with("NODEDOWN "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the runtime never saw `{{nodedown, {peer_node}}}` after the peer exited\n  \
                 runtime stdout: {:?}\n  runtime stderr: {}",
                runtime.transcript(),
                runtime.stderr_text(),
            )
        });
    assert_eq!(
        nodedown.trim(),
        format!("NODEDOWN {peer_node}"),
        "the runtime's node monitor reported the peer's own node name going down"
    );

    let status = runtime
        .wait_exit(EXIT_TIMEOUT)
        .unwrap_or_else(|| panic!("the runtime did not exit within {EXIT_TIMEOUT:?}"));
    assert_eq!(
        status.code(),
        Some(0),
        "the `run` action exits 0 once the fixture returns\n  stdout: {:?}\n  stderr: {}",
        runtime.transcript(),
        runtime.stderr_text(),
    );
    // The `run` action prints the returned term after the process returns; the fixture's
    // last observable step is the `{nodedown, …}` it returned `done` on.
    let Some(result) = runtime.wait_for(|line| line.trim() == "done", Duration::from_secs(5)) else {
        panic!(
            "the `run` action printed no `done` result\n  runtime stdout: {:?}\n  runtime stderr: \
             {}",
            runtime.transcript(),
            runtime.stderr_text(),
        )
    };
    assert_eq!(result.trim(), "done", "the fixture returned after the nodedown");
}

/// **The peer's own `nodedown`**: it `monitor_node/2`s the runtime, and killing the runtime
/// must deliver `{nodedown, RustNode}` to it.
///
/// This is the opposite direction of the previous test's node monitor, and the two together
/// are what make the `nodedown` claim symmetric rather than one-sided.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn the_peer_watches_the_rust_node_go_down() {
    let Some(harness) = Harness::new("nodedown") else {
        return;
    };
    let Some(fixture) = harness.compile_fixture() else {
        return;
    };

    let rust_name = unique('r');
    let peer_name = unique('o');
    let cookie = unique_cookie();
    let rust_node = format!("{rust_name}@{}", harness.host);
    let peer_node = format!("{peer_name}@{}", harness.host);

    let mut runtime = harness
        .spawn_runtime(&rust_name, &cookie, &fixture, LINGER_SPEC, None)
        .unwrap_or_else(|error| panic!("cannot start {BINARY} under setsid: {error}"));
    let Some(ready) = runtime.wait_for(|line| line.starts_with("READY "), READY_TIMEOUT) else {
        support::skip_loudly(
            "dist_two_vm",
            &format!(
                "the runtime printed no `READY` line within {READY_TIMEOUT:?}\n  command: {}\n  \
                 stdout: {:?}\n  stderr: {}",
                runtime.command,
                runtime.transcript(),
                runtime.stderr_text()
            ),
        );
        return;
    };
    assert_eq!(ready.trim(), format!("READY {rust_node}"));
    harness.report_registration(&rust_name);

    let mut peer = harness
        .spawn_peer(&peer_name, &cookie, ORACLE_WATCH_EVAL, &rust_node)
        .unwrap_or_else(|error| panic!("cannot start the system `erl` peer under setsid: {error}"));

    let Some(pong) = peer.wait_for(|line| line.starts_with("PONG "), LINE_TIMEOUT) else {
        panic!(
            "`net_adm:ping('{rust_node}')` never answered `pong`\n  peer command: {}\n  peer \
             stdout: {:?}\n  peer stderr: {}\n  runtime stdout: {:?}\n  runtime stderr: {}",
            peer.command,
            peer.transcript(),
            peer.stderr_text(),
            runtime.transcript(),
            runtime.stderr_text(),
        )
    };
    assert_eq!(pong.trim(), format!("PONG {peer_node}"));

    let watching = peer
        .wait_for(|line| line.starts_with("WATCHING "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the peer never armed `erlang:monitor_node({rust_node}, true)`\n  peer stdout: \
                 {:?}\n  peer stderr: {}\n  runtime stdout: {:?}\n  runtime stderr: {}",
                peer.transcript(),
                peer.stderr_text(),
                runtime.transcript(),
                runtime.stderr_text(),
            )
        });
    assert_eq!(
        watching.trim(),
        "WATCHING true",
        "`erlang:monitor_node(<runtime>, true)` answered `true` on the peer"
    );

    // Take the runtime away — the peer's monitor is the only thing that can report it, and
    // the group kill is the same path a crash takes.
    runtime.kill_group();
    assert!(
        runtime.wait_exit(KILL_GRACE).is_some(),
        "the runtime's process group is gone after the kill"
    );

    let nodedown = peer
        .wait_for(|line| line.starts_with("NODEDOWN "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the peer never saw `{{nodedown, {rust_node}}}` after the runtime was killed\n  \
                 peer stdout: {:?}\n  peer stderr: {}",
                peer.transcript(),
                peer.stderr_text(),
            )
        });
    assert_eq!(
        nodedown.trim(),
        format!("NODEDOWN {rust_node}"),
        "the peer's node monitor reported the runtime's own node name going down"
    );
    let status = peer
        .wait_exit(EXIT_TIMEOUT)
        .unwrap_or_else(|| panic!("the peer did not exit within {EXIT_TIMEOUT:?}"));
    assert_eq!(
        status.code(),
        Some(0),
        "the peer halted 0 after reporting the nodedown\n  peer stdout: {:?}\n  peer stderr: {}",
        peer.transcript(),
        peer.stderr_text(),
    );
}

// ---------------------------------------------------------------------------------------
// Preconditions: toolchain discovery (filesystem only) + the private `epmd` + the fixture
// ---------------------------------------------------------------------------------------

/// Everything one test needs before it can start a VM pair: the discovered tools, the
/// test's own scratch directory, the derived host name and the private `epmd`.
///
/// Declared in drop order: the `epmd` daemon is stopped **before** the scratch directory
/// (its cwd) is removed.
struct Harness {
    /// The system `erl` — the peer (resolved by [`support::resolve_oracle`]'s rules).
    oracle: PathBuf,
    /// The system `erlc` — compiles the fixture at test time.
    erlc: PathBuf,
    /// `setsid`, which puts every child in its own session (see the module docs).
    setsid: PathBuf,
    /// `kill`, which delivers the group `TERM`/`KILL` a `Child::kill` cannot.
    kill: PathBuf,
    /// The short hostname **both** nodes live under, derived exactly as `beam_rt`'s
    /// `dist::local_hostname` derives it (`/proc/sys/kernel/hostname`, then `$HOSTNAME`,
    /// then `localhost`) — never by shelling out to `hostname`.
    host: String,
    /// The private `epmd` this run registered with (dropped → stopped).
    epmd_daemon: PrivateEpmd,
    /// This test's own directory under `$TMPDIR` (the fixture's output directory, the
    /// children's cwd, and the `-pa` the runtime is given).
    scratch: Scratch,
}

impl Harness {
    /// Discovers the toolchain, makes the scratch directory and starts the private `epmd`,
    /// or `None` after a **loud skip** naming exactly what is missing.
    fn new(label: &str) -> Option<Harness> {
        let Some(oracle) = support::require_oracle("dist_two_vm") else {
            return None;
        };
        let erlc = tool("erlc")?;
        let epmd_program = tool("epmd")?;
        let setsid = tool("setsid")?;
        let kill = tool("kill")?;
        let scratch = Scratch::new(label)?;
        let host = local_hostname();
        let epmd_daemon = PrivateEpmd::start(&epmd_program, &setsid, &kill, scratch.path())?;
        Some(Harness {
            oracle,
            erlc,
            setsid,
            kill,
            host,
            epmd_daemon,
            scratch,
        })
    }

    /// Compiles [`FIXTURE_ERL`] into `<scratch>/two_vm.beam` with the system `erlc`, or
    /// `None` after a loud skip when the compiler fails.
    fn compile_fixture(&self) -> Option<PathBuf> {
        let source = self.scratch.path().join(format!("{FIXTURE_MODULE}.erl"));
        if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
            support::skip_loudly(
                "dist_two_vm",
                &format!("cannot write the fixture source {}: {error}", source.display()),
            );
            return None;
        }
        let output = Command::new(&self.erlc)
            .arg("-o")
            .arg(self.scratch.path())
            .arg(&source)
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                support::skip_loudly(
                    "dist_two_vm",
                    &format!(
                        "`{}` exited with {}: {}",
                        self.erlc.display(),
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                );
                return None;
            }
            Err(error) => {
                support::skip_loudly(
                    "dist_two_vm",
                    &format!("cannot run `{}`: {error}", self.erlc.display()),
                );
                return None;
            }
        }
        let beam = self.scratch.path().join(format!("{FIXTURE_MODULE}.beam"));
        if !beam.is_file() {
            support::skip_loudly(
                "dist_two_vm",
                &format!("`erlc` produced no {}", beam.display()),
            );
            return None;
        }
        Some(beam)
    }

    /// The environment every child gets: the **private** `epmd` port, so neither VM can
    /// touch (or be seen by) any other node on the machine.
    fn child_env(&self) -> Vec<(&'static str, String)> {
        vec![("ERL_EPMD_PORT", self.epmd_daemon.port.to_string())]
    }

    /// Reports — never asserts — whether the runtime's short name really appeared in *this*
    /// run's private `epmd`, and whether the machine's default `epmd` knows it instead.
    ///
    /// This is the one precondition the peer's `pong` cannot distinguish from the others: a
    /// runtime that ignores `ERL_EPMD_PORT` registers on 4369 and is invisible to the peer,
    /// which looks exactly like "the runtime does not serve `net_kernel`". Both facts end up
    /// in the failure message, so a red test is attributable rather than mysterious.
    fn report_registration(&self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(port) = beam_dist::epmd::lookup_port_at(&self.epmd_daemon.addr, name.as_bytes())
            {
                eprintln!(
                    "note: the private `epmd` on {} resolved {name} to listener port {port}",
                    self.epmd_daemon.addr
                );
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(POLL);
        }
        let elsewhere = beam_dist::epmd::lookup_port_at(
            beam_dist::EPMD_DEFAULT_ADDR,
            name.as_bytes(),
        );
        eprintln!(
            "warning: {name} never appeared in the private `epmd` on {} within 15s — is \
             ERL_EPMD_PORT honoured for the runtime's ALIVE2 registration? (default `epmd` on \
             {} says {:?})",
            self.epmd_daemon.addr,
            beam_dist::EPMD_DEFAULT_ADDR,
            elsewhere
        );
    }
    /// Starts the runtime under test as a `-sname` node on the `run` route.
    ///
    /// `peer_node` (when given) is the peer's full `node@host` atom, passed to the fixture
    /// as a **double-quoted** `run` argument — a char list, so no atom-charset question
    /// reaches the command line.
    fn spawn_runtime(
        &self,
        name: &str,
        cookie: &str,
        fixture: &Path,
        spec: &str,
        peer_node: Option<&str>,
    ) -> std::io::Result<ChildVm> {
        let mut args: Vec<String> = vec![
            "-noshell".into(),
            "-sname".into(),
            name.to_string(),
            "-setcookie".into(),
            cookie.to_string(),
            "-pa".into(),
            self.scratch.path().display().to_string(),
            "run".into(),
            fixture.display().to_string(),
            spec.to_string(),
        ];
        if let Some(peer) = peer_node {
            args.push(format!("\"{peer}\""));
        }
        ChildVm::spawn(
            "runtime",
            &self.setsid,
            &self.kill,
            Path::new(BINARY),
            &args,
            &self.child_env(),
            Some(self.scratch.path()),
        )
    }

    /// Starts the system `erl` peer with `eval` as its `-eval` expression, `@TARGET@`
    /// replaced by `target` (the runtime's full `node@host` atom).
    fn spawn_peer(
        &self,
        name: &str,
        cookie: &str,
        eval: &str,
        target: &str,
    ) -> std::io::Result<ChildVm> {
        let eval = eval.replace("@TARGET@", target);
        let args: Vec<String> = vec![
            "-sname".into(),
            name.to_string(),
            "-setcookie".into(),
            cookie.to_string(),
            "-noshell".into(),
            "-eval".into(),
            eval,
        ];
        ChildVm::spawn(
            "peer",
            &self.setsid,
            &self.kill,
            &self.oracle,
            &args,
            &self.child_env(),
            Some(self.scratch.path()),
        )
    }
}

/// `name` on `$PATH` (**filesystem only** — the tool is never run to detect it), or a loud
/// skip naming it.
fn tool(name: &str) -> Option<PathBuf> {
    match support::find_on_path(name) {
        Some(path) => Some(path),
        None => {
            support::skip_loudly(
                "dist_two_vm",
                &format!("no `{name}` on $PATH (this suite drives a real two-VM pair)"),
            );
            None
        }
    }
}

/// The short hostname Erlang itself derives for a `-sname` node, derived the way
/// [`beam_rt`]'s `dist::local_hostname` derives it: `/proc/sys/kernel/hostname`, then
/// `$HOSTNAME`, then the literal `localhost`. Deliberately **not** `hostname(1)` — the
/// harness never shells out to discover a fact the runtime reads from a file.
fn local_hostname() -> String {
    if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let host = contents.trim();
        if !host.is_empty() {
            return host.to_string();
        }
    }
    match std::env::var("HOSTNAME") {
        Ok(host) if !host.is_empty() => host,
        _ => "localhost".to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Unique names
// ---------------------------------------------------------------------------------------

/// A per-call counter, so the two tests (which cargo may run concurrently) never pick the
/// same node names or cookie.
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// A process-unique, **lowercase-alphanumeric** node-name fragment: `-sname` lowercases and
/// validates the name, and a leftover node from an earlier run must never collide.
fn unique(tag: char) -> String {
    format!(
        "twovm{tag}{}{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    )
}

/// A process-unique cookie (never the developer's `~/.erlang.cookie`: every node is given
/// `-setcookie` explicitly).
fn unique_cookie() -> String {
    format!(
        "ck{}{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    )
}

// ---------------------------------------------------------------------------------------
// The test's own scratch directory
// ---------------------------------------------------------------------------------------

/// A private directory under `$TMPDIR`, unique per call (label + pid + sequence), removed on
/// drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// Creates `$TMPDIR/beam_rt_two_vm_<label>_<pid>_<n>`, or `None` after a loud skip.
    fn new(label: &str) -> Option<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_two_vm_{label}_{}_{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => Some(Scratch { dir }),
            Err(error) => {
                support::skip_loudly(
                    "dist_two_vm",
                    &format!(
                        "cannot create the scratch directory {}: {error}",
                        dir.display()
                    ),
                );
                None
            }
        }
    }

    /// The directory itself (the fixture's output directory and the children's cwd).
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------------------
// The private `epmd`
// ---------------------------------------------------------------------------------------

/// A private `epmd` daemon on a private port, stopped on drop.
///
/// The port comes from an ephemeral bind that is released immediately, so it collides with
/// no other test (nor with a developer's own `epmd` on 4369), and every child is handed
/// `ERL_EPMD_PORT=<port>` so both VMs register with, and resolve through, *this* daemon.
struct PrivateEpmd {
    /// The port the daemon listens on.
    port: u16,
    /// `127.0.0.1:<port>` — the address every probe and lookup uses.
    addr: String,
    /// The resolved `kill`, used to reclaim the detached daemon.
    kill: PathBuf,
}

impl PrivateEpmd {
    /// Starts `epmd -daemon -port <free port>` under `setsid -w` with `ERL_EPMD_PORT` set
    /// and its cwd inside `scratch`, then waits (bounded) for it to accept a connection.
    ///
    /// `None` after a loud skip when no free port is available, the daemon fails to start,
    /// or it never accepts a connection.
    fn start(epmd: &Path, setsid: &Path, kill: &Path, scratch: &Path) -> Option<PrivateEpmd> {
        let Some(port) = free_port() else {
            support::skip_loudly(
                "dist_two_vm",
                "no free loopback port for the private `epmd`",
            );
            return None;
        };
        let addr = format!("127.0.0.1:{port}");
        let output = Command::new(setsid)
            .arg("-w")
            .arg(epmd)
            .arg("-daemon")
            .arg("-port")
            .arg(port.to_string())
            .env("ERL_EPMD_PORT", port.to_string())
            .current_dir(scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                support::skip_loudly(
                    "dist_two_vm",
                    &format!(
                        "`{} -daemon -port {port}` exited with {}: {}",
                        epmd.display(),
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                );
                return None;
            }
            Err(error) => {
                support::skip_loudly(
                    "dist_two_vm",
                    &format!("cannot run `{}`: {error}", epmd.display()),
                );
                return None;
            }
        }
        let daemon = PrivateEpmd {
            port,
            addr,
            kill: kill.to_path_buf(),
        };
        // `epmd -daemon` returns as soon as the daemon parent has forked, so readiness is
        // "it accepts a connection" rather than "the command returned".
        let deadline = Instant::now() + EPMD_START_TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect(&daemon.addr).is_ok() {
                return Some(daemon);
            }
            thread::sleep(POLL);
        }
        support::skip_loudly(
            "dist_two_vm",
            &format!(
                "the private `epmd` never accepted a connection on {} within {EPMD_START_TIMEOUT:?}",
                daemon.addr
            ),
        );
        // A daemon that came up too late (or on a port we cannot reach) must still not be
        // left running.
        daemon.stop();
        None
    }

    /// Stops the daemon and proves its port is closed.
    ///
    /// A daemonized `epmd` detaches into its own session, so the group kill that reclaims
    /// the VMs cannot reach it — and OTP's daemon refuses `KILL_REQ`/`STOP_REQ` from an
    /// ordinary client (measured: `epmd -stop`/`-kill` answer `local epmd responded with
    /// <>` unless it was started with `-relaxed_command_check`). So the daemon is found by
    /// its **own argv** ([`epmd_pid_on_port`], which only our private `-port <port>` matches)
    /// and signalled by pid.
    fn stop(&self) {
        // Best effort: have the daemon drop every registration first.
        if let Ok(mut client) = beam_dist::epmd::connect(&self.addr) {
            let _ = client.kill();
        }
        let Some(pid) = epmd_pid_on_port(self.port) else {
            if port_closed(&self.addr) {
                return;
            }
            eprintln!(
                "warning: `epmd` on {} accepts connections but no daemon with `-port {}` was \
                 found in /proc to stop",
                self.addr, self.port
            );
            return;
        };
        signal(&self.kill, &["-TERM", &pid.to_string()]);
        if wait_for_port_closed(&self.addr, EPMD_STOP_TIMEOUT) {
            return;
        }
        signal(&self.kill, &["-KILL", &pid.to_string()]);
        if wait_for_port_closed(&self.addr, EPMD_STOP_TIMEOUT) {
            return;
        }
        eprintln!(
            "warning: the private `epmd` (pid {pid}) still holds {} after TERM and KILL",
            self.addr
        );
    }
}

impl Drop for PrivateEpmd {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A free loopback port: bound as `:0`, read back, and released at once.
fn free_port() -> Option<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

/// Whether nothing accepts connections on `addr` any more.
fn port_closed(addr: &str) -> bool {
    TcpStream::connect(addr).is_err()
}

/// Polls [`port_closed`] for up to `timeout`.
fn wait_for_port_closed(addr: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if port_closed(addr) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
}

/// The pid of the `epmd` daemon started with `-port <port>`, found by reading
/// `/proc/<pid>/cmdline` — a plain filesystem scan, so only *this* run's private daemon (the
/// only one whose argv carries the port) can match.
fn epmd_pid_on_port(port: u16) -> Option<u32> {
    let wanted = port.to_string();
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        let Some(program) = args.first() else {
            continue;
        };
        let base = program.rsplit('/').next().unwrap_or(program);
        if base != "epmd" {
            continue;
        }
        if args
            .windows(2)
            .any(|pair| pair[0] == "-port" && pair[1] == wanted)
        {
            return Some(pid);
        }
    }
    None
}

/// Runs `<kill> <args…>` with the **resolved** `kill` binary (never a shell builtin).
fn signal(kill: &Path, args: &[&str]) {
    let _ = Command::new(kill)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------------------
// A VM child in its own session
// ---------------------------------------------------------------------------------------

/// One VM (`beam_rt` or the system `erl`) running as `setsid -w <program> …`: its own
/// session, hence its own process group, so its `SIGUSR1`/`SIGTERM` traffic can never reach
/// the test binary or a sibling test.
///
/// The child's stdout is drained line by line into a channel (so a `READY`/`PONG`/`GOT` line
/// can be awaited as it appears) *and* kept in a transcript for failure messages; stderr is
/// collected the same way. Drop kills the **group** (`TERM`, a bounded grace, then `KILL`)
/// and reaps, so nothing survives a failed assertion.
struct ChildVm {
    /// The exact command line, so a failure names what was run.
    command: String,
    /// The `setsid -w …` process — with `setsid`'s direct exec, the VM itself.
    child: Child,
    /// The process group to signal: this child's pid when `/proc/<pid>/stat` says it really
    /// leads its own group (`setsid` execs directly, so it does), else `None`.
    pgid: Option<u32>,
    /// The resolved `kill` used for the group signals.
    kill: PathBuf,
    /// The child's stdout, one line at a time.
    lines: Receiver<String>,
    /// Every stdout line so far (failure diagnostics).
    transcript: Arc<Mutex<Vec<String>>>,
    /// Every stderr line so far (failure diagnostics).
    stderr: Arc<Mutex<String>>,
}

impl ChildVm {
    /// Spawns `setsid -w <program> <args…>` with `env` applied and `cwd` set, and starts the
    /// two reader threads.
    fn spawn(
        label: &str,
        setsid: &Path,
        kill: &Path,
        program: &Path,
        args: &[String],
        env: &[(&str, String)],
        cwd: Option<&Path>,
    ) -> std::io::Result<ChildVm> {
        let mut command = Command::new(setsid);
        command.arg("-w").arg(program);
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let command_line = format!(
            "setsid -w {} {}",
            program.display(),
            args.iter()
                .map(|arg| if arg.contains(' ') || arg.contains('\n') {
                    format!("{arg:?}")
                } else {
                    arg.clone()
                })
                .collect::<Vec<_>>()
                .join(" ")
        );
        let mut child = command.spawn()?;
        let pid = child.id();
        // `setsid` calls `setsid(2)` and execs the program, so the child *is* the session and
        // group leader; read the group back from /proc rather than assuming it (a `setsid`
        // that forked would leave `pgid == pid` false, and then only the process itself is
        // signalled — never an unrelated group).
        let pgid = wait_for_group_leader(pid);
        if pgid.is_none() {
            eprintln!(
                "warning: {label}: `setsid -w` did not leave pid {pid} as its own group leader; \
                 the group signal will be skipped"
            );
        }
        let stdout = child.stdout.take().expect("the child's stdout is piped");
        let stderr_pipe = child.stderr.take().expect("the child's stderr is piped");

        let transcript = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&transcript);
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(mut buffer) = sink.lock() {
                    buffer.push(line.clone());
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                let Ok(line) = line else { break };
                match sink.lock() {
                    Ok(mut buffer) => {
                        buffer.push_str(&line);
                        buffer.push('\n');
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(ChildVm {
            command: command_line,
            child,
            pgid,
            kill: kill.to_path_buf(),
            lines,
            transcript,
            stderr,
        })
    }

    /// Waits (bounded) for a stdout line satisfying `pred`, discarding earlier lines.
    ///
    /// Returns `None` on the timeout, or when the child's stdout reached EOF first.
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

    /// Every stdout line so far (failure diagnostics).
    fn transcript(&self) -> Vec<String> {
        self.transcript
            .lock()
            .map(|buffer| buffer.clone())
            .unwrap_or_default()
    }

    /// The stderr collected so far (failure diagnostics).
    fn stderr_text(&self) -> String {
        self.stderr.lock().map(|text| text.clone()).unwrap_or_default()
    }

    /// Polls for the child's exit, bounded by `timeout`; the child is **reaped** when it has
    /// exited.
    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL);
        }
    }

    /// Signals `signal` to this child's **process group** (a no-op when the group could not
    /// be established; see [`ChildVm::pgid`]).
    fn signal_group(&self, signal: &str) {
        let Some(pgid) = self.pgid else { return };
        let _ = Command::new(&self.kill)
            .arg(signal)
            .arg("--")
            .arg(format!("-{pgid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// Terminates the whole group: `TERM`, a bounded grace, then `KILL`.
    fn kill_group(&mut self) {
        if self.wait_exit(Duration::ZERO).is_some() {
            return;
        }
        self.signal_group("-TERM");
        if self.wait_exit(KILL_GRACE).is_some() {
            return;
        }
        self.signal_group("-KILL");
        let _ = self.wait_exit(KILL_GRACE);
    }
}

impl Drop for ChildVm {
    fn drop(&mut self) {
        self.kill_group();
        // Belt and braces: a child whose group could not be established is killed directly,
        // and a child that already exited is reaped here.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls `/proc/<pid>/stat` briefly for a process that really leads its own group, and
/// answers that group id.
///
/// `setsid` execs the program it was given, so the child *is* the session leader and
/// `pgrp == pid`; the poll exists only to let that change land after the spawn.
fn wait_for_group_leader(pid: u32) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(pgid) = leader_pgid(pid) {
            return Some(pgid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// `pid`'s process group id when it leads that group, read from `/proc/<pid>/stat`
/// (field 5, `pgrp`, after the parenthesised command that may itself contain spaces).
fn leader_pgid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let mut fields = stat[close + 1..].split_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    let pgrp: u32 = fields.next()?.parse().ok()?;
    (pgrp == pid).then_some(pgrp)
}
