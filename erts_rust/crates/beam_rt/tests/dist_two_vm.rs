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

//! **Two-VM distribution** (contract A.5): a real system-ERTS node and this Rust ERTS
//! runtime, **live at once**, sharing one **private** `epmd`, with the traffic that
//! crosses between them asserted on **both** sides.
//!
//! The other A.5 suites each pin one half of a link from a different angle —
//! [`tests/dist.rs`](../dist.rs) the `'N'` handshake, [`tests/dist_client.rs`](../dist_client.rs)
//! this runtime dialling out, [`tests/live_inbound.rs`](../live_inbound.rs) an inbound
//! `DOP_REG_SEND` into a running local process, [`tests/live_signals.rs`](../live_signals.rs)
//! the outbound signal families — all of them over the **default** `epmd` port, and all
//! of them from *one* VM's point of view. This suite is the integration proof: two VMs at
//! once, and every observability claim is a line printed by **one VM about the other**.
//!
//! The harness itself (the private `epmd`, the session-isolated children, the scratch
//! directory, the skip policy) lives in [`tests/support/dist.rs`](../support/dist.rs) —
//! one home for every two-VM suite, including the rust↔rust one that shares this file's
//! fixture.
//!
//! # What this file asserts, end to end
//!
//! ## The `-eval` peer: the inbound families
//!
//! 1. the runtime boots as a `-sname` node, registers with the private `epmd`, and its
//!    fixture prints a `READY <node>` line once its registered name exists;
//! 2. the system `erl` peer resolves it **by name** — `net_adm:ping('rust…@host')`
//!    answers **`pong`** (which is a real `gen:call` to the remote `net_kernel`, so the
//!    runtime has to serve that endpoint, not merely accept a socket);
//! 3. **one message each way**: the peer `DOP_REG_SEND`s `{greeting, PeerNode, 42}` to
//!    the runtime's registered name `two_vm_peer` and the runtime `!`s
//!    `{reply, RustNode, 43}` to the peer's registered name `oracle_peer`; **both**
//!    payloads are printed by the receiving VM and compared, verbatim, to the exact
//!    expected term;
//! 4. **`nodedown`** in both directions — the runtime's own
//!    `erlang:monitor_node(PeerNode, true)` reports `{nodedown, PeerNode}` when the peer
//!    exits ([`the_oracle_pings_the_rust_node_and_a_message_crosses_each_way`]), and the
//!    peer's `erlang:monitor_node(RustNode, true)` reports `{nodedown, RustNode}` when
//!    the runtime is killed ([`the_peer_watches_the_rust_node_go_down`]).
//!
//! ## The actor peer: the distributed **signal** families, peer-observed
//!
//! The `-eval` expression a `spawn_peer` call carries cannot run on a `beam_rt` peer, so
//! the signal families use a **compiled** fixture instead — the *responder* / *actor*
//! pair below. The peer runs `two_vm:<actor>/1`, the same Erlang source on a real `erl`
//! **and** on `beam_rt` (`support::dist::PeerKind`), and what each family asserts is what
//! the **peer** observed about **our** process:
//!
//! | test | the peer does | the peer must report |
//! |---|---|---|
//! | [`a_real_erl_peer_links_to_our_process_and_traps_our_exit`] | `link/1` + `trap_exit`, then dies our victim with `boom` | `EXIT_MATCH pid=true reason=boom`, and the `{'EXIT', Pid, boom}` whose pid **is** the pid we printed |
//! | [`a_real_erl_peer_traps_our_exit_with_the_reason_term_it_asked_for`] | the same, with the **compound** reason `{shutdown,{custom,7}}` | the reason *value*, not merely the event |
//! | [`a_real_erl_peer_unlinks_first_so_our_exit_is_not_delivered`] | `unlink/1` first (with a barrier proving *our* node processed it), then dies the victim | `NOEXIT`, and no `EXIT` line at all |
//! | [`a_real_erl_peer_monitors_our_process_and_gets_the_peer_minted_down`] | `erlang:monitor(process, Victim)`, then dies it | `{'DOWN', Ref, process, Victim, boom}` in a clause that **binds the same `Ref`** |
//! | [`a_real_erl_peer_demonitors_before_our_death_so_no_down_arrives`] | `demonitor(Ref, [flush])` first | `NODOWN`, and no `DOWN` line at all |
//! | [`our_pid_survives_a_round_trip_through_a_real_erl_peer`] | echoes a pid term back | our pid arrives intact (`IDENTITY true`, the same text) |
//!
//! Every one of them is `#[ignore]`d: they need a whole system Erlang toolchain (`erl`,
//! `erlc`, `epmd`) plus `setsid` and `kill`, and they really do spawn two VMs.
//!
//! # The responder / actor protocol
//!
//! Our node runs the **responder** (`run <fixture.beam> two_vm:<responder>/1 "<peer
//! node>"`); the peer runs the **actor** (`erl … -run two_vm <actor> "<our node>"`, or
//! `beam_rt … run <fixture.beam> two_vm:<actor>/1 "<our node>"`). The message exchange is
//! the same in every signal test, one numbered step per message:
//!
//! 1. **responder → `epmd`**: the runtime registers its node, then prints `READY <node>`,
//!    `PEER <peer node>`, and (for the signal families) `VICTIM <pid>` — the *local* pid
//!    the peer is about to act on. Its own `erlang:monitor(process, Victim)` makes it
//!    report `VICTIM_DOWN <reason>` independently of the peer, which is what makes the
//!    negative controls non-vacuous.
//! 2. **actor → responder, `{hello, ActorPid}`** to the registered name `two_vm_peer`,
//!    retried on a bounded schedule (a `DOP_REG_SEND` to a name that is not registered
//!    *yet* is silently dropped — by the oracle too): a registration race, not a bug.
//! 3. **responder → actor, `{victim, VictimPid}`** — every greeting is answered.
//! 4. **actor, locally**: arms its own view of the relationship — `link/1` with
//!    `process_flag(trap_exit, true)`, or `erlang:monitor/2` — and prints it
//!    (`LINKED <pid>` / `REF <ref>`).
//! 5. **actor → responder, `{barrier, ActorPid}`**; **responder → actor,
//!    `{barrier_ack, ResponderPid}`**. Signals from one peer travel in order on its
//!    link, so the ack proves *our* node has already applied step 4's `DOP_LINK` /
//!    `DOP_MONITOR_P` / `DOP_UNLINK_ID` / `DOP_DEMONITOR_P` — without it the death below
//!    could race the relationship and make a *pass* meaningless.
//! 6. **actor → victim, `{die, Reason}`** — a plain `DOP_SEND` to a local pid of ours.
//!    The victim is a **spawned child**, never the responder: the responder is the run
//!    action's target, and if it exited the runtime would exit before the outbound
//!    `DOP_EXIT` / `DOP_MONITOR_P_EXIT` it just routed could be flushed.
//! 7. **our node → actor, `{'EXIT', Victim, Reason}` or `{'DOWN', Ref, process, Victim,
//!    Reason}`** — the outbound signal family under test. The actor prints what it
//!    matched and, where a pid is involved, the *identity* it matched (a `=:=` guard).
//! 8. **actor → responder, `{saw, Verdict}`**; **responder → actor, `{bye, done}`**;
//!    **actor → responder, `{thanks, ActorPid}`**. The responder prints `PEER_SAW
//!    <verdict>`, then stays alive until the actor confirms — the ordering that keeps a
//!    VM from exiting out from under the last message of the other.
//!
//! Three rules the fixture keeps so that the **same** source can run on a `beam_rt` peer:
//!
//! * no pid *literal* (`<0.1.0>` is not parseable outside a shell) — pids cross only as
//!   terms in messages;
//! * no `timer:sleep` (portability across both runtimes): a wait is `receive … after T`;
//! * no printing of a **remote** pid through `~p` (`erlang:pid_to_list/1` of an external
//!   pid is a typed capability gap in this runtime — `beam_bifs`'s `external_channel_gap` —
//!   so the fixture's `pid_text/1` falls back to a marker rather than fabricating a
//!   channel number; the pid *identity* assertions never depend on the text).
//!
//! # The pid text a peer can print, and why it is normalized
//!
//! `erlang:pid_to_list/1` renders `<node.number.serial>`, but for a pid that arrived from
//! another node the first component is the receiving VM's **node-table index** — the
//! oracle printed `<10304.35.2>` for our `<0.35.2>`, and the index differs per VM and per
//! run. `node/1` names the node, and both facts come from the peer itself, so the
//! fixture's `canonical_pid/2` substitutes the node name and yields the canonical
//! `<twovmr…@host.35.2>` the *other* VM printed — a normalization of two observed facts,
//! never an invention. The tests then derive that same string from our own printed pid
//! (`Pair::external_pid_text`), so the peer's view of our pid is compared **term for
//! term** with the pid we printed.
//!
//! # What the runtime side must hold up (each is exercised below)
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
//!   reaches the peer's registered name.
//! * **an accepted link can signal**: a `DOP_LINK`/`DOP_MONITOR_P` the peer sends is
//!   recorded against our local process, a `DOP_UNLINK_ID`/`DOP_DEMONITOR_P` removes it
//!   again, and when that process dies the runtime routes `DOP_EXIT`/`DOP_MONITOR_P_EXIT`
//!   back over the **same** link, with the reason term intact and the peer's own reference
//!   echoed.
//! * **`erlang:monitor_node(Node, true)`** answers `true` for a connected node and delivers
//!   `{nodedown, Node}` to the monitoring process when the link dies, in **both**
//!   directions.
//! * the fixture's `receive … after T` keeps the run's scheduler pump stepping (a bare
//!   `receive` leaves the pump quiescent, which is why every wait in the fixture is
//!   deadline-bounded: with an empty run queue, nothing to poll and no pending timer,
//!   `Runtime::run_scheduled` stops with its honest `blocked` diagnostic).
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_rt --test dist_two_vm -- --ignored --nocapture
//! ```
//!
//! To run one of them: append `--exact <test-name>` after the `--`. `--nocapture` matters:
//! every child's transcript is echoed on failure *and* on every loud skip. Each test is a
//! two-VM session of a few seconds (the two negative controls spend 3 s in their own
//! deliberately-expiring wait); `--test-threads=1` keeps a loaded host's timings honest.
//! The whole eight-test file runs in ~18 s on an idle host.

// The two-VM harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

use support::dist::{
    Harness, PeerKind, BINARY, EXIT_TIMEOUT, KILL_GRACE, LINE_TIMEOUT, READY_TIMEOUT, unique,
    unique_cookie,
};
use support::fixture::{FIXTURE_ERL, FIXTURE_MODULE};
use support::pair::Pair;

use std::time::Duration;

/// This suite's name — every loud skip and every failure message names it.
const BANNER: &str = "dist_two_vm";

// ---------------------------------------------------------------------------------------
// The fixture (compiled at test time with the system `erlc`; never committed)
// ---------------------------------------------------------------------------------------

// The Erlang module both two-VM suites run as their peer — this suite on a system `erl`,
// `tests/dist_two_rust.rs` on a second `beam_rt` — lives in `support::fixture`, one home for
// the source and its protocol docs (including the rust↔rust section's identity rule: a
// `beam_rt` peer cannot render a *remote* pid/reference at all, so it asserts `=:=` instead).

/// The runtime's entry point for the full exchange.
const EXCHANGE_SPEC: &str = "two_vm:run/1";

/// The runtime's entry point for the passive nodedown case.
const LINGER_SPEC: &str = "two_vm:linger/0";

/// The responder for the peer-orchestrated signal families: spawns the victim and stays
/// alive until the actor has finished observing.
const PEER_DRIVES_SPEC: &str = "two_vm:peer_drives/1";

/// The responder for the identity round trip.
const IDENTITY_SPEC: &str = "two_vm:identity/1";

/// The responders of the **remote-victim** family (the peer hosts the victim and our
/// node acts on it): a link, a monitor, the monitor's negative control, the two
/// `erlang:exit/2` reasons, and the two alias directions.
const RV_LINK_SPEC: &str = "two_vm:rv_link/1";
const RV_MONITOR_SPEC: &str = "two_vm:rv_monitor/1";
const RV_DEMONITOR_SPEC: &str = "two_vm:rv_demonitor/1";
const RV_EXIT_KILL_SPEC: &str = "two_vm:rv_exit_kill/1";
const RV_EXIT_NORMAL_SPEC: &str = "two_vm:rv_exit_normal/1";
const RV_OUR_ALIAS_SPEC: &str = "two_vm:rv_our_alias/1";
const RV_THEIR_ALIAS_SPEC: &str = "two_vm:rv_their_alias/1";

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
// The two original tests
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
    let Some(mut harness) = Harness::new("exchange", "dist_two_vm") else {
        return; // every precondition already reported its loud skip
    };
    let Some(fixture) = harness.compile_fixture(FIXTURE_ERL, FIXTURE_MODULE) else {
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
    let Some(mut harness) = Harness::new("nodedown", "dist_two_vm") else {
        return;
    };
    let Some(fixture) = harness.compile_fixture(FIXTURE_ERL, FIXTURE_MODULE) else {
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
// The responder/actor session: one live pair per test
// ---------------------------------------------------------------------------------------

// `support::pair::Pair` (imported above) is the session itself — both VMs plus every
// helper. It is shared with the rust↔rust suite, which is why its constructor takes the peer
// kind and the suite banner.

// ---------------------------------------------------------------------------------------
// The distributed signal families, observed by the peer (contract A.5)
// ---------------------------------------------------------------------------------------

/// **The peer links to our process and traps the exit our process dies with.**
///
/// The actor `link/1`s the victim *we* spawned (with `process_flag(trap_exit, true)`),
/// waits for a barrier that proves our node recorded the `DOP_LINK`, then sends the victim
/// `{die, boom}`. Our node must route a `DOP_EXIT` back over the **same** (peer-dialled)
/// link, carrying the reason and our own pid, and the actor must receive
/// `{'EXIT', Victim, boom}` with the very pid we printed.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_real_erl_peer_links_to_our_process_and_traps_our_exit() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "link", PEER_DRIVES_SPEC, "link_and_trap") else {
        return; // every precondition already reported its loud skip
    };
    // The victim's pid, printed by **us** (a local pid, so `<0.number.serial>`).
    let victim = pair.runtime_text("VICTIM ");
    let victim_seen_from_peer = pair.external_pid_text(&victim, &pair.rust_node);

    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The pid travelled to the peer as a term and renders as the peer's own view of it.
    pair.actor_equals(
        "VICTIM_SEEN ",
        &format!("VICTIM_SEEN {victim_seen_from_peer}"),
    );
    pair.actor_equals("LINKED ", &format!("LINKED {victim_seen_from_peer}"));
    pair.actor_equals("BARRIER ", "BARRIER ok");
    // The exit our node routed: the reason value, and the `{'EXIT', Pid, Reason}` whose
    // pid is compared with `=:=` in the actor's own clause guard.
    pair.actor_equals("EXIT_MATCH ", "EXIT_MATCH pid=true reason=boom");
    pair.actor_equals(
        "EXIT_TERM ",
        &format!("EXIT_TERM {{'EXIT',{victim_seen_from_peer},boom}}"),
    );
    // Our own runtime saw the same victim die with the same reason (an independent,
    // local witness that the peer's `NOEXIT`-style negative controls rest on).
    pair.runtime_equals("VICTIM_DOWN ", "VICTIM_DOWN boom");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.finish();
}

/// **The reason *term* survives, not merely the event**: the same link exit, with the
/// victim dying of a compound reason (`{shutdown,{custom,7}}`) the actor chose.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_real_erl_peer_traps_our_exit_with_the_reason_term_it_asked_for() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "link_compound", PEER_DRIVES_SPEC, "link_and_trap_compound")
    else {
        return;
    };
    let victim = pair.runtime_text("VICTIM ");
    let victim_seen_from_peer = pair.external_pid_text(&victim, &pair.rust_node);

    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals(
        "VICTIM_SEEN ",
        &format!("VICTIM_SEEN {victim_seen_from_peer}"),
    );
    pair.actor_equals("LINKED ", &format!("LINKED {victim_seen_from_peer}"));
    pair.actor_equals("BARRIER ", "BARRIER ok");
    pair.actor_equals(
        "EXIT_MATCH ",
        "EXIT_MATCH pid=true reason={shutdown,{custom,7}}",
    );
    pair.actor_equals(
        "EXIT_TERM ",
        &format!("EXIT_TERM {{'EXIT',{victim_seen_from_peer},{{shutdown,{{custom,7}}}}}}"),
    );
    pair.runtime_equals("VICTIM_DOWN ", "VICTIM_DOWN {shutdown,{custom,7}}");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.finish();
}

/// **The negative control**: the actor `unlink/1`s the victim *before* it dies, so our
/// node must deliver **no** exit signal at all.
///
/// The barrier is what makes this meaningful: same-link signals are applied in order, so
/// the `{barrier_ack, …}` proves our node had already processed the `DOP_UNLINK_ID` when
/// the victim died. The runtime's own `VICTIM_DOWN boom` proves the victim really did die
/// during the actor's bounded wait — without it, "no exit arrived" would be vacuous.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_real_erl_peer_unlinks_first_so_our_exit_is_not_delivered() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "unlink", PEER_DRIVES_SPEC, "link_then_unlink") else {
        return;
    };
    let victim = pair.runtime_text("VICTIM ");
    let victim_seen_from_peer = pair.external_pid_text(&victim, &pair.rust_node);

    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals(
        "VICTIM_SEEN ",
        &format!("VICTIM_SEEN {victim_seen_from_peer}"),
    );
    pair.actor_equals("LINKED ", &format!("LINKED {victim_seen_from_peer}"));
    pair.actor_equals("UNLINKED", "UNLINKED");
    pair.actor_equals("BARRIER ", "BARRIER ok");
    // The bounded wait printed its verdict: nothing arrived.
    pair.actor_equals("NOEXIT", "NOEXIT");
    // The victim died — our own local monitor is the witness — and nothing was routed.
    pair.runtime_equals("VICTIM_DOWN ", "VICTIM_DOWN boom");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW no_exit");
    pair.assert_absent(
        &pair.actor,
        &["EXIT_TERM", "EXIT_MATCH", "UNEXPECTED_EXIT", "EXIT_TIMEOUT"],
        "the peer unlinked before the victim died, so no exit signal may reach it",
    );
    pair.actor_equals("BYE ", "BYE done");
    pair.finish();
}

/// **The peer monitors our process and receives the peer-minted `DOWN`.**
///
/// The actor arms `erlang:monitor(process, Victim)`, prints the reference it minted, then
/// dies the victim. Our node must route a `DOP_MONITOR_P_EXIT` that carries **that**
/// reference (the actor's receive clause binds the same `Ref`, so any other reference
/// falls through to its catch-all) together with our pid and the reason.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_real_erl_peer_monitors_our_process_and_gets_the_peer_minted_down() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "monitor", PEER_DRIVES_SPEC, "monitor_victim") else {
        return;
    };
    let victim = pair.runtime_text("VICTIM ");
    let victim_seen_from_peer = pair.external_pid_text(&victim, &pair.rust_node);

    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals(
        "VICTIM_SEEN ",
        &format!("VICTIM_SEEN {victim_seen_from_peer}"),
    );
    // The reference the **peer** minted, printed by the peer.
    let reference = pair.actor_text("REF ");
    pair.actor_equals("BARRIER ", "BARRIER ok");
    pair.actor_equals("DOWN_MATCH ", "DOWN_MATCH ref=bound pid=true reason=boom");
    // The whole `{'DOWN', …}` term: the reference and the pid are the very ones already
    // printed above, and the reason is the one the actor chose.
    pair.actor_equals(
        "DOWN_TERM ",
        &format!("DOWN_TERM {{'DOWN',{reference},process,{victim_seen_from_peer},boom}}"),
    );
    pair.runtime_equals("VICTIM_DOWN ", "VICTIM_DOWN boom");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.assert_absent(
        &pair.actor,
        &["DOWN_OTHER", "DOWN_TIMEOUT", "UNEXPECTED_DOWN"],
        "the peer's monitor matched its own reference",
    );
    pair.actor_equals("BYE ", "BYE done");
    pair.finish();
}

/// **The other monitor control**: the actor `demonitor(Ref, [flush])`s *before* the
/// victim dies, so no `DOWN` may arrive at all.
///
/// As in the unlink case the barrier carries the meaning: our node had already processed
/// the `DOP_DEMONITOR_P` when the victim died, and the runtime's own `VICTIM_DOWN boom`
/// shows that death really happened inside the actor's bounded wait.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_real_erl_peer_demonitors_before_our_death_so_no_down_arrives() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "demonitor", PEER_DRIVES_SPEC, "monitor_then_demonitor") else {
        return;
    };
    let victim = pair.runtime_text("VICTIM ");
    let victim_seen_from_peer = pair.external_pid_text(&victim, &pair.rust_node);

    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals(
        "VICTIM_SEEN ",
        &format!("VICTIM_SEEN {victim_seen_from_peer}"),
    );
    // The reference the peer minted (and then dropped), printed by the peer itself.
    let reference = pair.actor_text("REF ");
    pair.actor_equals("DEMONITORED", "DEMONITORED");
    pair.actor_equals("BARRIER ", "BARRIER ok");
    pair.actor_equals("NODOWN", "NODOWN");
    pair.runtime_equals("VICTIM_DOWN ", "VICTIM_DOWN boom");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW no_down");
    // The peer's reference occurs exactly once in its whole transcript: its own `REF` line.
    // A `{'DOWN', …}` naming it would be a second occurrence, so this catches a DOWN printed
    // in any shape at all, not only the shapes `assert_absent` lists.
    let occurrences = pair
        .actor
        .transcript()
        .iter()
        .filter(|line| line.contains(&reference))
        .count();
    assert_eq!(
        occurrences, 1,
        "the peer's reference {reference} must appear only in its own `REF` line, never in a DOWN\n{}",
        pair.diagnostics()
    );
    pair.assert_absent(
        &pair.actor,
        &["DOWN_TERM", "DOWN_MATCH", "UNEXPECTED_DOWN", "DOWN_TIMEOUT"],
        "the peer demonstrated its monitor before the victim died, so no DOWN may arrive",
    );
    pair.actor_equals("BYE ", "BYE done");
    pair.finish();
}

/// **Our pid survives the round trip**: the responder sends `self()` to the peer, the peer
/// echoes the *same term* back, and the responder compares it with `=:=`.
///
/// `IDENTITY true` is the assertion; `SELF`/`ECHO`/`ECHO_NODE` and the peer's `ECHOED_IN`
/// make a mismatch diagnosable (a pid that came back as a *different* term would print
/// different text, and one that lost its internal form here would fail the comparison
/// before it could be printed at all).
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn our_pid_survives_a_round_trip_through_a_real_erl_peer() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "identity", IDENTITY_SPEC, "echo_identity") else {
        return;
    };
    let me = pair.runtime_text("SELF ");

    pair.runtime_equals("SELF_NODE ", &format!("SELF_NODE {}", pair.rust_node));
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The pid term the peer received is the one we print, rendered from the peer's side.
    pair.actor_equals(
        "ECHOED_IN ",
        &format!("ECHOED_IN {}", pair.external_pid_text(&me, &pair.rust_node)),
    );
    pair.actor_equals("ECHO_SENT", "ECHO_SENT");
    // ..., and what came back is `=:=` our own pid, in our own rendering.
    pair.runtime_equals("IDENTITY ", "IDENTITY true");
    pair.runtime_equals("ECHO ", &format!("ECHO {me}"));
    pair.runtime_equals("ECHO_NODE ", &format!("ECHO_NODE {}", pair.rust_node));
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.finish();
}

// ---------------------------------------------------------------------------------------
// The remote-victim family: our node acts on a process the **peer** hosts (contract A.5)
// ---------------------------------------------------------------------------------------
//
// The six tests above have the peer observe *our* process (the actor links to, monitors and
// kills a victim we host). These seven are the opposite direction — the second half of the
// two-live-VM signal evidence: the peer hosts the victim, hands its pid over, and our node
// is the one that links, monitors, demonitors, exits and aliases. What is asserted is the
// same shape: every claim is a line one VM printed about the other, and every identity is a
// clause guard (`{'EXIT', Victim, …}`, `{'DOWN', MRef, …, Victim, …}`) or the peer's own
// `lists:member/2` over `process_info(Victim, links | monitored_by)` — never a pid *text*.
//
// The pid text both sides print is the peer's (`VICTIM_HOSTED <text>`), because this runtime
// cannot render a remote pid at all: `beam_bifs`'s printer has only the internal arm, and the
// external one is the typed capability gap the fixture's `pid_text/1` catches and replaces
// with `remote-pid`. So the peer — the one VM that can name that channel — reports the text
// in the handover (`{victim, V, Text}`) and both transcripts are compared against it.

/// **Our `erlang:link/1` of a real `erl` process, trapping that process' exit.**
///
/// Our responder arms `process_flag(trap_exit, true)` and `link/1`s the victim; the peer
/// proves the `DOP_LINK` landed (`PEER_LINK_SEEN true` — our pid *is* in the victim's own
/// `links` list) and only then forwards the death command, so the `{'EXIT', …}` below cannot
/// be a race. The trapped exit must arrive with **that** victim's pid (the clause binds it)
/// and the reason the peer was told to die with.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn our_link_to_a_real_erl_process_traps_that_process_exit() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "rv_link", RV_LINK_SPEC, "actor_rv_link") else {
        return; // every precondition already reported its loud skip
    };
    // The pid text the **peer** printed for its own victim, and the witness ref it minted.
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let victim = pair.actor_text("VICTIM_HOSTED ");
    let witness = pair.actor_text("WITNESS_REF ");

    // Our own arm line carries the peer-reported text, i.e. the pid we were handed is the
    // pid the peer reported.
    pair.runtime_equals("LINK_ARMED ", &format!("LINK_ARMED {victim}"));
    // The peer observed our `DOP_LINK` in the victim's link set before the death.
    pair.actor_equals("PEER_LINK_SEEN ", "PEER_LINK_SEEN true");
    pair.actor_equals("DIE_FORWARDED ", "DIE_FORWARDED boom");
    // Our node routed the exit back over the link: identity (the clause binds the pid) and
    // the reason, and the whole `{'EXIT', Pid, Reason}` term with the peer's own pid text.
    pair.runtime_equals("EXIT_MATCH ", "EXIT_MATCH pid=true reason=boom");
    pair.runtime_equals("EXIT_TERM ", &format!("EXIT_TERM {{'EXIT',{victim},boom}}"));
    // The peer's independent witness of the same death.
    pair.actor_equals("WITNESS_DOWN ", "WITNESS_DOWN reason=boom");
    pair.actor_equals(
        "WITNESS_TERM ",
        &format!("WITNESS_TERM {{'DOWN',{witness},process,{victim},boom}}"),
    );
    pair.assert_absent(
        &pair.runtime,
        &["EXIT_TIMEOUT", "DOWN_OTHER"],
        "the trapped exit matched the linked victim",
    );
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.finish();
}

/// **Our `erlang:monitor/2` of a real `erl` process receives the `DOWN` carrying our own
/// reference** — the inbound `DOP_MONITOR_P_EXIT` path.
///
/// Our responder prints the reference it minted, then waits in a clause that **binds** it
/// (`{'DOWN', MRef, process, Victim, Reason}`), so a DOWN carrying any other reference falls
/// through to the catch-all and is reported as `DOWN_OTHER`. The peer proves the
/// `DOP_MONITOR_P` landed (`PEER_MONITORED_BY true`) before dying the victim.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn our_monitor_of_a_real_erl_process_receives_the_monitor_exit_down() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "rv_monitor", RV_MONITOR_SPEC, "actor_rv_monitor") else {
        return;
    };
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let victim = pair.actor_text("VICTIM_HOSTED ");
    // The reference **we** minted, printed by us.
    let reference = pair.runtime_text("REF ");

    pair.actor_equals("PEER_MONITORED_BY ", "PEER_MONITORED_BY true");
    pair.actor_equals("DIE_FORWARDED ", "DIE_FORWARDED boom");
    // The clause that matched had the reference bound, and the pid is the victim's.
    pair.runtime_equals("DOWN_MATCH ", "DOWN_MATCH ref=bound pid=true reason=boom");
    // The whole term: the reference is the very text we printed, the pid is the peer's own
    // rendering of the victim, and the reason is the one the peer died it with.
    pair.runtime_equals(
        "DOWN_TERM ",
        &format!("DOWN_TERM {{'DOWN',{reference},process,{victim},boom}}"),
    );
    pair.assert_absent(
        &pair.runtime,
        &["DOWN_OTHER", "DOWN_TIMEOUT"],
        "the DOWN matched our own reference and the monitored pid",
    );
    pair.actor_equals("WITNESS_DOWN ", "WITNESS_DOWN reason=boom");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.finish();
}

/// **The negative control for the monitor**: `erlang:demonitor(Ref, [flush])` before the
/// death suppresses the `DOWN` completely.
///
/// Non-vacuity needs the peer to watch the relationship *change*, which it does on its own
/// process: our pid **appears** in the victim's `monitored_by` (`PEER_MONITOR_SEEN true`),
/// our node demonitors, and the pid is **gone** again (`PEER_MONITOR_GONE true`) before the
/// victim dies (`VICTIM_DIED reason=boom`). So `NODOWN` is a demonstrated absence: this runtime
/// *did* have a monitor of that process, and it removed it.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn our_demonitor_of_a_real_erl_process_suppresses_the_down() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "rv_demonitor", RV_DEMONITOR_SPEC, "actor_rv_demonitor") else {
        return;
    };
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let _victim = pair.actor_text("VICTIM_HOSTED ");
    let _witness = pair.actor_text("WITNESS_REF ");
    // The reference we minted and then dropped.
    let reference = pair.runtime_text("REF ");

    pair.actor_equals("PEER_MONITOR_SEEN ", "PEER_MONITOR_SEEN true");
    pair.runtime_equals("DEMONITORED", "DEMONITORED");
    pair.actor_equals("PEER_MONITOR_GONE ", "PEER_MONITOR_GONE true");
    pair.actor_equals("DIE_FORWARDED ", "DIE_FORWARDED boom");
    // The bounded wait expired with nothing in it.
    pair.runtime_equals("NODOWN", "NODOWN");
    // ..., and the death really did happen inside that window (the peer's own witness).
    pair.actor_equals("VICTIM_DIED ", "VICTIM_DIED reason=boom");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["DOWN_TERM", "DOWN_MATCH", "UNEXPECTED_DOWN", "DOWN_TIMEOUT"],
        "we demonstrated the monitor before the victim died, so no DOWN may arrive",
    );
    pair.assert_no_substring(
        &pair.runtime,
        "'DOWN'",
        "our side demonstrated its monitor and then removed it, so no DOWN term may appear",
    );
    pair.assert_no_substring(
        &pair.actor,
        "'DOWN'",
        "the peer's own witness prints only the reason, so no DOWN term may appear",
    );
    // Our reference occurs exactly once in our whole transcript: its own `REF` line.
    let occurrences = pair
        .runtime
        .transcript()
        .iter()
        .filter(|line| line.contains(&reference))
        .count();
    assert_eq!(
        occurrences, 1,
        "our reference {reference} must appear only in its own `REF` line, never in a DOWN\n{}",
        pair.diagnostics()
    );
    pair.finish();
}

/// **Our `erlang:exit(Victim, kill)` kills a real `erl` process.**
///
/// `exit/2` answers `true` on our side (`EXIT_SENT true`) and the peer's own `spawn_monitor`ed
/// witness reports the death — ERTS rewrites the untrappable `kill` to the reason `killed`
/// (`erl_proc_sig_queue.c`'s exit-policy arm). Our node holds no link to the victim here, so
/// nothing comes back to us: the peer's witness is the whole assertion.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn our_remote_exit_kill_kills_a_real_erl_process() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "rv_kill", RV_EXIT_KILL_SPEC, "actor_rv_exit_kill") else {
        return;
    };
    pair.runtime_equals("EXIT_SENT ", "EXIT_SENT true");
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let victim = pair.actor_text("VICTIM_HOSTED ");
    let witness = pair.actor_text("WITNESS_REF ");

    pair.actor_equals("WITNESS_DOWN ", "WITNESS_DOWN reason=killed");
    pair.actor_equals(
        "WITNESS_TERM ",
        &format!("WITNESS_TERM {{'DOWN',{witness},process,{victim},killed}}"),
    );
    pair.assert_absent(
        &pair.actor,
        &["WITNESS_OTHER", "WITNESS_TIMEOUT"],
        "the peer's witness matched its own reference and the victim",
    );
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.finish();
}

/// **Our `erlang:exit(Victim, normal)` leaves a real `erl` process alive** — the non-vacuity
/// control for the `kill` above, over a byte-identical session.
///
/// ERTS ignores an exit signal whose reason is `normal` when the target does not trap exits,
/// so the peer's witness gets no `DOWN` inside its bounded window and the peer then proves the
/// victim is still *running* with a round trip (`VICTIM_ALIVE true`, a `{ping, …, Ref}` /
/// `{pong, Ref}` pair) — not merely unreaped.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn our_remote_exit_normal_leaves_a_real_erl_process_alive() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "rv_normal", RV_EXIT_NORMAL_SPEC, "actor_rv_exit_normal") else {
        return;
    };
    pair.runtime_equals("EXIT_SENT ", "EXIT_SENT true");
    // The peer's greeting comes before it spawns the victim, so its `PONG` is read first.
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let _victim = pair.actor_text("VICTIM_HOSTED ");
    let _witness = pair.actor_text("WITNESS_REF ");

    // The bounded window expired with no DOWN, and the probe round trip answered.
    pair.actor_equals("VICTIM_ALIVE ", "VICTIM_ALIVE true");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    // No death report of any shape from either side.
    pair.assert_absent(
        &pair.actor,
        &[
            "VICTIM_DOWN",
            "WITNESS_DOWN",
            "WITNESS_TERM",
            "WITNESS_OTHER",
            "WITNESS_TIMEOUT",
        ],
        "a `normal` exit signal to a non-trapping process must not kill it",
    );
    pair.assert_no_substring(
        &pair.actor,
        "'DOWN'",
        "the victim survived, so the peer's witness never reported a DOWN term",
    );
    pair.finish();
}

/// **A real `erl` peer sends to *our* alias and the payload lands in our mailbox** — the
/// inbound `DOP_ALIAS_SEND` (the C's `erts_proc_sig_send_dist_altact_msg`, `dist.c:2563`).
///
/// Our responder mints the alias (`erlang:alias([])`), prints the reference it holds and hands
/// the **term** to the peer over the wire; the peer sends `{payload, Node, 4242}` to it. The
/// payload must arrive verbatim in *our* mailbox, resolved from the alias reference to its
/// owner (this process) — `RECEIVED {payload,<peer node>,4242}`.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_real_erl_peer_sends_to_our_alias_and_the_message_lands_in_our_mailbox() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "rv_alias_in", RV_OUR_ALIAS_SPEC, "actor_rv_our_alias") else {
        return;
    };
    // The alias reference we minted, printed by us (`#Ref<0.…>`).
    let reference = pair.runtime_text("ALIAS ");

    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The peer's own rendering of the reference it received: our reference with our node name
    // in the channel slot, i.e. the alias travelled as a term and names *this* node. The peer's
    // leading component is its node-table index for us, so it is normalized to the node name
    // first (exactly the substitution `canonical_pid/2` makes for pids).
    let seen = pair.actor_text("ALIAS_SEEN ");
    assert_eq!(
        pair.canonical_ref_text(&seen, &pair.rust_node),
        pair.external_ref_text(&reference, &pair.rust_node),
        "the peer's own rendering of the alias reference names our node\n{}",
        pair.diagnostics()
    );
    pair.actor_equals("ALIAS_SENT", "ALIAS_SENT");
    // The payload, verbatim, out of our own mailbox.
    pair.runtime_equals(
        "RECEIVED ",
        &format!("RECEIVED {{payload,{},4242}}", pair.actor_node),
    );
    pair.assert_absent(
        &pair.runtime,
        &["ALIAS_TIMEOUT", "NOHANDOVER"],
        "the alias send was delivered to its owner",
    );
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.finish();
}

/// **We send to a real `erl` peer's alias and the payload lands in the peer's mailbox** — the
/// outbound `DOP_ALIAS_SEND`.
///
/// The peer mints the alias and hands the reference to us; our responder sends
/// `{payload, Node, 7}` to it and prints the send's own return value (the message term). The
/// peer must print that payload out of *its* mailbox, plus its own identity check that the
/// sender is the responder it handed the alias to.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn we_send_to_a_real_erl_peers_alias_and_it_lands_in_the_peers_mailbox() {
    let Some(pair) = Pair::start(PeerKind::Oracle, BANNER, "rv_alias_out", RV_THEIR_ALIAS_SPEC, "actor_rv_their_alias") else {
        return;
    };
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The reference the **peer** minted, printed by the peer.
    let _reference = pair.actor_text("ALIAS ");
    // Our send's return value: the message term, naming our own node.
    pair.runtime_equals(
        "ALIAS_SENT ",
        &format!("ALIAS_SENT {{payload,{},7}}", pair.rust_node),
    );
    // The payload, verbatim, out of the peer's own mailbox.
    pair.actor_equals(
        "RECEIVED ",
        &format!("RECEIVED {{payload,{},7}}", pair.rust_node),
    );
    pair.actor_equals("PAYLOAD_FROM_RESPONDER ", "PAYLOAD_FROM_RESPONDER true");
    pair.assert_absent(
        &pair.actor,
        &["PAYLOAD_TIMEOUT", "NOALIAS"],
        "the alias send was delivered to its owner on the peer",
    );
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.finish();
}
