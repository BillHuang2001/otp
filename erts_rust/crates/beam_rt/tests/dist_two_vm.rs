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
//! Payloads whose *size* is the point (a payload far larger than the receiver's nursery,
//! delivered through the heap-fragment read path) live in the sibling
//! [`tests/dist_payload_matrix.rs`](../dist_payload_matrix.rs), which reuses this suite's
//! harness.
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
//! # Isolation and the skip policy
//!
//! Every child (the private `epmd`, the `erl` peer, the `beam_rt` runtime) runs as its own
//! session under `setsid -w` and is reclaimed by a group `TERM`/`KILL`, and the pair shares
//! one `epmd` on an ephemeral port — the rules, and the reason each one exists, are
//! [`support::two_vm`](support/two_vm.rs)'s module docs. Every precondition (a missing
//! tool, an `epmd` that never comes up, a runtime that never prints `READY`) is a **loud
//! skip**, never a silent pass, and the fixture is compiled at test time with the system
//! `erlc` into the test's own scratch directory.
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

// The shared harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

use std::time::Duration;

use support::two_vm::{
    unique, unique_cookie, Harness, BINARY, EXIT_TIMEOUT, KILL_GRACE, LINE_TIMEOUT, READY_TIMEOUT,
};

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

/// The suite name every loud skip reports.
const BANNER: &str = "dist_two_vm";

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
    let Some(harness) = Harness::new(BANNER, "exchange") else {
        return; // every precondition already reported its loud skip
    };
    let Some(fixture) = harness.compile_fixture(FIXTURE_MODULE, FIXTURE_ERL) else {
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
            BANNER,
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
    let Some(harness) = Harness::new(BANNER, "nodedown") else {
        return;
    };
    let Some(fixture) = harness.compile_fixture(FIXTURE_MODULE, FIXTURE_ERL) else {
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
            BANNER,
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
