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

//! **Two `beam_rt` nodes** (contract A.5): the runtime's own distribution on *both* ends
//! of a link — its acceptor and its connector, its inbound decode and its outbound encode,
//! with no system ERTS anywhere in the picture.
//!
//! [`tests/dist_two_vm.rs`](../dist_two_vm.rs) is the same evidence against a **system
//! `erl`** peer, and its harness (`tests/support/dist.rs`) is reused here unchanged: two
//! live VMs, one private `epmd`, `setsid`-isolated children, every claim a line one VM
//! printed about the other. Only the peer's **kind** differs
//! ([`PeerKind::Rust`](support::dist::PeerKind)), so the two suites are the same protocol
//! run against the two halves of the same contract:
//!
//! | suite | peer | what a pass proves |
//! |---|---|---|
//! | `dist_two_vm` | system `erl` | this runtime's wire format and signals agree with **ERTS** |
//! | `dist_two_rust` (this file) | `beam_rt` | this runtime's wire format and signals agree with **itself** |
//!
//! Self-agreement is not a tautology, and that is the point of this file: the two VMs run
//! the same code but they do **not** run the same role. One is the `run` action's target
//! (the *responder*, reached by a `-pa`-loaded fixture), the other a second process with
//! its own `epmd` name, its own atom table, its own node table and its own creation. A
//! term therefore has to be encoded by one VM's encoder and decoded by the other VM's
//! decoder, an identifier minted here has to become an *external* identifier there and
//! back to an internal one when it returns, and a signal has to be routed by the sender's
//! link table and applied by the receiver's signal queue. Any bit-level asymmetry between
//! the two roles — an encode/decode round trip that is not an involution, a pid whose
//! internal word does not survive the trip, a signal family served on one side of the wire
//! but not the other — is visible here and invisible to a suite that only ever talks to
//! ERTS, because a real peer is a fixture that is *known* to be right.
//!
//! # A `beam_rt` peer cannot render a *remote* identifier — so the assertions are
//! # term-identity, not text
//!
//! `erlang:pid_to_list/1` of a pid that arrived from another node, and `~p`/`~w` of an
//! external pid or reference, are a typed capability gap in this runtime: an external
//! identifier's leading component is the *receiving* VM's node-table index, which no term
//! carries, so `beam_bifs`'s `external_channel_gap` answers `error:unsupported_bif` and the
//! printer has no external arm. A capability gap is **uncatchable by design** (its reason
//! atom is never interned, so a program's own `try … catch` cannot swallow it), which
//! means the fixture's `pid_text/1`/`ref_text/1` fallbacks do **not** rescue a `beam_rt`
//! peer: printing one would terminate that VM with exit 3.
//!
//! Every actor and responder this file drives therefore asserts identity by **pattern
//! match and `=:=`** and prints only booleans, atoms and reasons:
//!
//! ```text
//! EXIT_MATCH pid=true reason=boom          % the clause bound the linked pid
//! EXIT_IDENTITY true                       % the guard's own `V =:= Victim`
//! DOWN_MATCH ref=bound pid=true reason=…   % `{'DOWN', MRef, process, Victim, R}` matched
//! IDENTITY true                            % the echoed pid IS `self()`
//! ```
//!
//! That is **equal in strength** to the oracle suite's canonical-text comparisons —
//! `pid_to_list` text is only a rendering of an identity, and here the identity itself is
//! compared — and **different in shape**: `dist_two_vm` compares `<node.number.serial>`
//! strings (with the node-table index normalized to the node name), this file compares the
//! terms. Where a pid or reference crosses the wire twice (out and back), the identity
//! check is also a bit-level check: a word that came back different *is* a different pid.
//!
//! The one rendering this file does assert is a **local** one — our own pid as our own
//! `~p` prints it (`SELF`/`ECHO`, [`our_pid_survives_a_round_trip_through_a_rust_peer`]):
//! the term goes out to the peer and comes back, and it must land in this runtime's mailbox
//! as the very same `Eterm` word, i.e. the pid's node part decoded back into the internal
//! form. A renderer divergence there is a *decode* divergence.
//!
//! # What the two VMs must hold up (each is exercised below)
//!
//! * **the connector meets the acceptor, between two `beam_rt` nodes.** Our node dials the
//!   peer (`net_adm:ping/1` → `Runtime::dist_connect` → `DistClient`) while the peer's
//!   `DistNode` accepts — the first time both halves of *this* runtime face each other.
//!   The reverse direction is exercised by every other test in this file: each actor's
//!   first act is `net_adm:ping/1` of *our* node, i.e. this runtime's connector against
//!   this runtime's acceptor with the roles swapped.
//! * **`DOP_REG_SEND`/`DOP_SEND` both ways**, `DOP_LINK` + `DOP_EXIT`, `DOP_MONITOR_P` +
//!   `DOP_MONITOR_P_EXIT`, `DOP_DEMONITOR_P`, `DOP_ALIAS_SEND` — routed by the sender's
//!   `DistLinks` (one table over both halves) and honoured by the receiver.
//! * **identifier round trips**: a pid inside `{'EXIT', Pid, Reason}` and inside
//!   `{'DOWN', Ref, process, Pid, Reason}` must be `=:=` the pid that was handed over
//!   earlier, and a pid echoed through the peer must come back `=:=` `self()`.
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_rt --test dist_two_rust -- --ignored --nocapture --test-threads=1
//! ```
//!
//! To run one of them, append `--exact <test-name>` after the `--`. `--nocapture` matters:
//! every child's transcript is echoed on failure *and* on every loud skip. Every test is a
//! two-VM session of a few seconds — the demonitor negative control deliberately spends 2 s
//! in its own expiring window — and `--test-threads=1` keeps a loaded host's timings honest.
//!
//! Nothing here needs the system Erlang toolchain at **run** time (both VMs are this
//! binary), but the fixture is still **compiled** by `erlc` at test time, so a host without
//! `erlc`/`epmd`/`setsid`/`kill` reports a **loud skip** rather than a pass.

// The two-VM harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

use support::dist::PeerKind;
use support::pair::Pair;

/// This suite's name — every loud skip and every failure message names it.
const BANNER: &str = "dist_two_rust";

// ---------------------------------------------------------------------------------------
// The responders our node runs (`run <fixture.beam> <spec> "<peer node>"`), one per test
// ---------------------------------------------------------------------------------------

/// [`two_rust_nodes_ping_each_other`]: `net_adm:ping/1` in both directions.
const RR_PING_SPEC: &str = "two_vm:rr_ping/1";
/// [`a_rust_peer_links_to_our_process_and_our_exit_reaches_it`]: the peer links to a victim
/// we host, and our node's `DOP_EXIT` reaches it.
const PEER_DRIVES_SPEC: &str = "two_vm:peer_drives/1";
/// [`our_monitor_of_a_rust_peer_process_receives_the_down`]: our `DOP_MONITOR_P` of the
/// peer's victim, and the inbound `DOP_MONITOR_P_EXIT`.
const RV_MONITOR_SPEC: &str = "two_vm:rv_monitor/1";
/// [`our_demonitor_of_a_rust_peer_process_suppresses_the_down`]: the monitor's negative
/// control — `demonitor(Ref, [flush])` before the death.
const RV_DEMONITOR_SPEC: &str = "two_vm:rv_demonitor/1";
/// [`our_remote_exit_kill_kills_a_process_on_a_rust_peer`]: our `erlang:exit/2`.
const RV_EXIT_KILL_SPEC: &str = "two_vm:rv_exit_kill/1";
/// [`an_alias_created_on_one_rust_node_delivers_from_the_other`]: our alias, the peer's
/// send.
const RR_OUR_ALIAS_SPEC: &str = "two_vm:rr_our_alias/1";
/// [`our_pid_survives_a_round_trip_through_a_rust_peer`]: our `self()` out and back.
const IDENTITY_SPEC: &str = "two_vm:identity/1";

// ---------------------------------------------------------------------------------------
// 1. The handshake, in both directions
// ---------------------------------------------------------------------------------------

/// **`net_adm:ping/1` answers `pong` in both directions between two `beam_rt` nodes.**
///
/// This is the first time this runtime's *connector* meets this runtime's *acceptor*: our
/// responder (`rr_ping/1`) dials the peer **before any link exists** — nothing has been
/// sent to the peer, so `DistLinks` has no entry and the dial is the only thing that can
/// create one — the peer's `DistNode` accepts, and the `'N'` handshake completes between
/// the two halves of the same implementation.
///
/// The reverse direction is the peer's own `net_adm:ping/1`, run *after* it has answered
/// our greeting (so its outcome cannot be confused with our dial's): the peer's connector
/// against our acceptor. `PING_OURS pong` and `PING_BACK pong` are therefore two
/// independent verdicts, one per direction, each produced by the VM that made the call.
///
/// A `pang` in either place is a real finding and the test fails on it — the failure names
/// the direction and dumps both transcripts — never smoothed over. (Every other test in
/// this file starts with the peer's `net_adm:ping/1` of *our* node and asserts its `PONG`
/// line, so the reverse direction is covered seven more times.)
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn two_rust_nodes_ping_each_other() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_ping",
        RR_PING_SPEC,
        "actor_rr_ping",
    ) else {
        return; // every precondition already reported its loud skip
    };
    // Our connector → the peer's acceptor. Nothing had been sent to the peer, so this dial
    // is what creates the link (a failed `net_adm:ping/1` is `pang`, never a raise, so the
    // responder's bounded retry loop covers the peer's startup).
    pair.runtime_equals("PING_OURS ", "PING_OURS pong");
    // Our greeting reached the peer's registered name over the link our dial opened, and
    // what it carried was a pid (the peer cannot render it, so it checks `is_pid/1`).
    pair.actor_equals("HELLO_IS_PID ", "HELLO_IS_PID true");
    // The reverse direction: the peer's connector → our acceptor.
    pair.actor_equals("PING_BACK ", "PING_BACK pong");
    // The peer's verdict, and our acknowledgement of it.
    pair.runtime_equals("PEER_SAW ", "PEER_SAW both_directions");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.actor,
        &["NOVISITOR", "PINGFAIL"],
        "the greeting and both pings completed inside their bounds",
    );
    // Both exit codes, the peer's `ACTOR_DONE` and our `done` result.
    pair.finish();
}

// ---------------------------------------------------------------------------------------
// 2-5. The signal families, with the peer hosting the process we act on, and our node
//      hosting the process the peer acts on
// ---------------------------------------------------------------------------------------

/// **A `beam_rt` peer links to a process *we* host, and our `DOP_EXIT` reaches it.**
///
/// The actor traps exits, `link/1`s a victim our responder spawned, and waits for a barrier
/// that proves our node recorded the `DOP_LINK` before the victim is told to die. Our node
/// must then route a `DOP_EXIT` back over the peer-dialled link with the pid and the
/// **reason term** intact.
///
/// The reason is compound (`{shutdown,{custom,7}}`) so the assertion covers the value, not
/// merely the event: a bare atom would pass for any atom. `EXIT_IDENTITY true` is the
/// actor's own `V =:= Victim` guard — the pid shipped to the peer as a term and came back
/// inside `{'EXIT', Pid, Reason}`, so a decode that produced a *different* pid fails here
/// rather than being masked by a rendering. Our own `VICTIM_DOWN` is the local witness
/// that the victim really died inside the actor's bounded wait.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn a_rust_peer_links_to_our_process_and_our_exit_reaches_it() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_link",
        PEER_DRIVES_SPEC,
        "actor_link_and_trap",
    ) else {
        return;
    };
    // The victim's pid, printed by **us** (a local pid of ours, so `<0.number.serial>`).
    let victim = pair.runtime_text("VICTIM ");
    assert!(
        victim.starts_with('<') && victim.ends_with('>'),
        "our responder prints a local pid: {victim}\n{}",
        pair.diagnostics()
    );
    // The peer's connector → our acceptor (its first act).
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The pid travelled to the peer as a term; it cannot render it, so it proves the type.
    pair.actor_equals("VICTIM_IS_PID ", "VICTIM_IS_PID true");
    // It trapped, linked, and our node processed the `DOP_LINK` before the death: the
    // barrier is a same-link round trip, so `BARRIER ok` orders the two.
    pair.actor_equals("LINK_ARMED", "LINK_ARMED");
    pair.actor_equals("BARRIER ok", "BARRIER ok");
    // The exit our node routed: the guard's own `=:=`, then the reason *value*.
    pair.actor_equals("EXIT_IDENTITY ", "EXIT_IDENTITY true");
    pair.actor_equals("EXIT_REASON ", "EXIT_REASON {shutdown,{custom,7}}");
    // Our own witness of the same death, with the same reason term.
    pair.runtime_equals("VICTIM_DOWN ", "VICTIM_DOWN {shutdown,{custom,7}}");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.actor,
        &["EXIT_OTHER_PID", "EXIT_TIMEOUT", "NOVICTIM"],
        "the trapped exit matched the linked victim inside the bound",
    );
    pair.finish();
}

/// **Our `erlang:monitor/2` of a `beam_rt` peer's process receives the `DOWN`** — the
/// inbound `DOP_MONITOR_P_EXIT` between two `beam_rt` nodes.
///
/// Our responder mints the reference, prints it (a **local** reference, so ours to render),
/// and waits in a clause that **binds** it — `{'DOWN', MRef, process, Victim, Reason}` — so
/// a DOWN carrying any other reference falls through to the catch-all and is reported as
/// `DOWN_OTHER`. The peer proves the `DOP_MONITOR_P` landed (`PEER_MONITORED_BY true`, read
/// off its own `process_info(Victim, monitored_by)`) before it dies the victim, so the
/// DOWN cannot be a race.
///
/// The two identity claims are the reference (ours, echoed back through the peer's
/// `DOP_MONITOR_P_EXIT`) and the pid (the peer's victim, which we were handed as a term).
/// The peer's own `WITNESS_*` lines are its independent view of the death.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn our_monitor_of_a_rust_peer_process_receives_the_down() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_monitor",
        RV_MONITOR_SPEC,
        "actor_rv_monitor",
    ) else {
        return;
    };
    let reference = pair.runtime_text("REF ");
    // The peer's connector → our acceptor, then its own view of the victim it hosts.
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let victim = pair.actor_text("VICTIM_HOSTED ");
    let witness = pair.actor_text("WITNESS_REF ");
    // The peer observed our `DOP_MONITOR_P` in the victim's own `monitored_by` list.
    pair.actor_equals("PEER_MONITORED_BY ", "PEER_MONITORED_BY true");
    pair.actor_equals("DIE_FORWARDED ", "DIE_FORWARDED boom");
    // The clause that matched had the reference bound, and the pid is the victim's.
    pair.runtime_equals("DOWN_MATCH ", "DOWN_MATCH ref=bound pid=true reason=boom");
    // The whole term, with the reference **we** minted and the pid **the peer** rendered:
    // both VMs' facts in one line, so the term and the rendering agree.
    pair.runtime_equals(
        "DOWN_TERM ",
        &format!("DOWN_TERM {{'DOWN',{reference},process,{victim},boom}}"),
    );
    // The peer's independent witness of the same death (both identifiers are local there).
    pair.actor_equals("WITNESS_DOWN ", "WITNESS_DOWN reason=boom");
    pair.actor_equals(
        "WITNESS_TERM ",
        &format!("WITNESS_TERM {{'DOWN',{witness},process,{victim},boom}}"),
    );
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["DOWN_OTHER", "DOWN_TIMEOUT"],
        "the DOWN matched our own reference and the monitored pid",
    );
    pair.finish();
}

/// **The negative control for the monitor**: `erlang:demonitor(Ref, [flush])` before the
/// death suppresses the `DOWN` completely.
///
/// Non-vacuity needs the peer to watch the relationship *change* on its own process, which
/// it does: our pid **appears** in the victim's `monitored_by` (`PEER_MONITOR_SEEN true`),
/// our node demonitors, and the pid is **gone** again (`PEER_MONITOR_GONE true`) before the
/// victim dies (`VICTIM_DIED reason=boom`). So `NODOWN` is a demonstrated absence: this
/// runtime *did* have a monitor of that process, and it removed it — the window it expires
/// in starts at the death notice, not at the demonitor, so an armed monitor's DOWN would
/// have to land inside it.
///
/// The mutation that proves the control is not vacuous is recorded in the test's own
/// comment: with the `erlang:demonitor/2` call removed from the fixture's `rv_demonitor/1`,
/// this test fails on `NODOWN` (an `UNEXPECTED_DOWN` arrives instead) — see the report.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn our_demonitor_of_a_rust_peer_process_suppresses_the_down() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_demonitor",
        RV_DEMONITOR_SPEC,
        "actor_rv_demonitor",
    ) else {
        return;
    };
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let _victim = pair.actor_text("VICTIM_HOSTED ");
    let _witness = pair.actor_text("WITNESS_REF ");
    // The reference we minted and then dropped.
    let reference = pair.runtime_text("REF ");
    // The peer's own demonstration that the monitor existed and then did not.
    pair.actor_equals("PEER_MONITOR_SEEN ", "PEER_MONITOR_SEEN true");
    pair.runtime_equals("DEMONITORED", "DEMONITORED");
    pair.actor_equals("PEER_MONITOR_GONE ", "PEER_MONITOR_GONE true");
    pair.actor_equals("DIE_FORWARDED ", "DIE_FORWARDED boom");
    // The bounded window, started at the death notice, expired with nothing in it.
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

/// **Our `erlang:exit(Victim, kill)` kills a process on a `beam_rt` peer.**
///
/// `exit/2` answers `true` on our side and the peer's own `spawn_monitor`ed witness reports
/// the death — ERTS rewrites the untrappable `kill` to the reason `killed`
/// (`erts/../erl_proc_sig_queue.c`'s exit-policy arm), so `killed` is the reason the peer
/// must print. Our node holds no link to the victim here, so nothing comes back to us: the
/// peer's witness is the whole assertion, and it asserts **terms**, not text (the witness
/// reference and the victim pid are both the peer's own local identifiers).
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn our_remote_exit_kill_kills_a_process_on_a_rust_peer() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_kill",
        RV_EXIT_KILL_SPEC,
        "actor_rv_exit_kill",
    ) else {
        return;
    };
    pair.runtime_equals("EXIT_SENT ", "EXIT_SENT true");
    // The peer's connector → our acceptor, then the victim it hosts.
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let victim = pair.actor_text("VICTIM_HOSTED ");
    let witness = pair.actor_text("WITNESS_REF ");
    // The peer's own witness: the reason ERTS substitutes for `kill`, and the whole
    // `{'DOWN', Ref, process, Pid, killed}` — the reference it minted, the pid it spawned.
    pair.actor_equals("WITNESS_DOWN ", "WITNESS_DOWN reason=killed");
    pair.actor_equals(
        "WITNESS_TERM ",
        &format!("WITNESS_TERM {{'DOWN',{witness},process,{victim},killed}}"),
    );
    pair.runtime_equals("PEER_SAW ", "PEER_SAW killed");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.actor,
        &["WITNESS_OTHER", "WITNESS_TIMEOUT"],
        "the peer's witness matched its own reference and the victim",
    );
    pair.finish();
}

// ---------------------------------------------------------------------------------------
// 6-7. The two round trips: a reference created here and used there, a pid of ours out and
//      back
// ---------------------------------------------------------------------------------------

/// **An alias created on one `beam_rt` node delivers a message sent from the other** — the
/// inbound `DOP_ALIAS_SEND` (the C's `erts_proc_sig_send_dist_altact_msg`, `dist.c:2563`).
///
/// Our responder mints the alias with `erlang:alias/1`, prints the reference it holds (a
/// local reference: ours to render) and hands the **term** to the peer over the wire; the
/// peer sends `{payload, Node, 4242}` to it. The payload must arrive verbatim in *our*
/// mailbox, resolved from the alias reference to its owner — `PAYLOAD_N 4242` — and its
/// sender must be the very pid we handed the alias to (`PAYLOAD_FROM_PEER true`, a `=:=`
/// over a pid that crossed the wire twice: out in a greeting, back inside the payload).
///
/// The peer asserts the send's own return value (`ALIAS_SENT true`): sending to a reference
/// is a real send, not a `badarg`.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn an_alias_created_on_one_rust_node_delivers_from_the_other() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_alias_in",
        RR_OUR_ALIAS_SPEC,
        "actor_rr_our_alias",
    ) else {
        return;
    };
    // The alias reference we minted, printed by us (`#Ref<0.…>`).
    let reference = pair.runtime_text("ALIAS ");
    assert!(
        reference.starts_with("#Ref<"),
        "our alias is a local reference, so we can render it: {reference}\n{}",
        pair.diagnostics()
    );
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The peer cannot render a reference it received, so it proves the type instead.
    pair.actor_equals("ALIAS_IS_REF ", "ALIAS_IS_REF true");
    pair.actor_equals("ALIAS_SENT ", "ALIAS_SENT true");
    // The payload, verbatim, out of our own mailbox — and its sender is the peer's pid.
    pair.runtime_equals("PAYLOAD_N ", "PAYLOAD_N 4242");
    pair.runtime_equals("PAYLOAD_FROM_PEER ", "PAYLOAD_FROM_PEER true");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW alias_sent");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["ALIAS_TIMEOUT", "NOVISITOR", "NOALIAS"],
        "the alias send was delivered to its owner",
    );
    // The reference appears only where we printed it: nothing echoed it back (an alias is
    // not a monitor, so no DOWN names it).
    let occurrences = pair
        .runtime
        .transcript()
        .iter()
        .filter(|line| line.contains(&reference))
        .count();
    assert_eq!(
        occurrences, 1,
        "our alias reference belongs to its own `ALIAS` line only\n{}",
        pair.diagnostics()
    );
    pair.finish();
}

/// **Our pid survives a round trip through a `beam_rt` peer**: the responder sends
/// `self()`, the peer echoes the *same term* back, and the responder compares it with
/// `=:=` and prints both renderings.
///
/// `IDENTITY true` is the assertion. It is the observable a broken inbound pid decode would
/// falsify: a pid of *this* node that arrives from the wire must decode back into the
/// internal form (channel `0`, our own creation) — anything else is a different `Eterm`
/// word, so `=:=` fails and the test fails. `ECHO` is our own `~p` of what came back, and it
/// must be the very text we printed for `SELF`; `SELF_NODE`/`ECHO_NODE` are both our node,
/// printed by us. (The peer prints only `ECHO_IS_PID` — a pid it received is *external*
/// there and it cannot render one.)
///
/// The same identity question on the **signal** path is asserted by
/// [`a_rust_peer_links_to_our_process_and_our_exit_reaches_it`] (`EXIT_IDENTITY true`) and
/// [`our_monitor_of_a_rust_peer_process_receives_the_down`] (`pid=true`): there the pid
/// rides inside a `{'EXIT', …}`/`{'DOWN', …}` term, here it rides inside an ordinary
/// message. Same decoder, three shapes.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn our_pid_survives_a_round_trip_through_a_rust_peer() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_identity",
        IDENTITY_SPEC,
        "actor_echo_identity",
    ) else {
        return;
    };
    let me = pair.runtime_text("SELF ");
    pair.runtime_equals("SELF_NODE ", &format!("SELF_NODE {}", pair.rust_node));
    // The peer's connector → our acceptor, then the pid it received (as a term).
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals("ECHO_IS_PID ", "ECHO_IS_PID true");
    pair.actor_equals("ECHO_SENT", "ECHO_SENT");
    // What came back is `=:=` our own pid — and renders as the very text we printed.
    pair.runtime_equals("IDENTITY ", "IDENTITY true");
    pair.runtime_equals("ECHO ", &format!("ECHO {me}"));
    pair.runtime_equals("ECHO_NODE ", &format!("ECHO_NODE {}", pair.rust_node));
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["PARENT_TIMEOUT", "NOIDENTITY"],
        "the pid came back and matched before the bound",
    );
    pair.finish();
}
